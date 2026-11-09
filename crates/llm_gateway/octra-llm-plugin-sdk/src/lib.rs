//! Authoring SDK for third-party LLM Gateway plugins.
//!
//! A plugin is a WebAssembly component that implements [`LlmPlugin`] and
//! declares its exported world with [`export_plugin!`]. The canonical ABI is
//! the JSON envelope in `wit/llm-plugin.wit`; this crate parses the envelope,
//! dispatches each stage and serialises the result back.
//!
//! The plugin crate keeps its own `wit/llm-plugin.wit` and lets
//! `cargo component` generate `src/bindings.rs`:
//!
//! ```ignore
//! mod bindings;
//!
//! struct MyPlugin { tag: String }
//!
//! impl octra_llm_plugin_sdk::LlmPlugin for MyPlugin {
//!     fn configure(config: serde_json::Value) -> octra_llm_plugin_sdk::Result<Self> {
//!         let tag = config["tag"].as_str().unwrap_or("default").to_string();
//!         Ok(Self { tag })
//!     }
//!
//!     fn on_request(
//!         &mut self,
//!         cx: &mut octra_llm_plugin_sdk::RequestContext<'_>,
//!     ) -> octra_llm_plugin_sdk::Result<octra_llm_plugin_sdk::Decision> {
//!         cx.headers.push(("x-plugin".to_string(), self.tag.clone()));
//!         Ok(octra_llm_plugin_sdk::Decision::Continue)
//!     }
//! }
//!
//! octra_llm_plugin_sdk::export_plugin!(MyPlugin, bindings);
//! ```
//!
//! Host functions (`log`, `now-unix`) are imported from the plugin's own
//! generated bindings, for example
//! `bindings::octra::llm_plugin::host::log(...)`.

use serde_json::Value;

pub use serde_json as __serde_json;
pub use serde_json::Value as __Value;

/// Plugin configuration or stage error. Returning it fails the stage and the
/// host applies the manifest `fail_policy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.message
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Gateway error returned when a stage rejects the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    pub status: u16,
    /// One of `invalid_request`, `authentication`, `permission`, `not_found`,
    /// `rate_limited`, `upstream`, `timeout`, `internal`, `canceled`.
    pub kind: String,
    pub code: Option<String>,
    pub message: String,
}

impl GatewayError {
    pub fn new(status: u16, kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            kind: kind.into(),
            code: None,
            message: message.into(),
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    #[doc(hidden)]
    pub fn to_json(&self) -> Value {
        Value::Object(
            [
                ("status".to_string(), Value::from(self.status)),
                ("kind".to_string(), Value::String(self.kind.clone())),
                ("message".to_string(), Value::String(self.message.clone())),
            ]
            .into_iter()
            .chain(
                self.code
                    .as_ref()
                    .map(|code| ("code".to_string(), Value::String(code.clone()))),
            )
            .collect(),
        )
    }
}

/// Stage outcome for request, provider-request, response and error stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Continue with the (possibly mutated) payload.
    Continue,
    /// Reject the request. On the error stage this replaces the error.
    Reject(GatewayError),
}

/// Stream-stage outcome, which can also split or reorder events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDecision {
    Continue,
    Replace(Vec<Value>),
    Reject(GatewayError),
}

/// Client-facing request stage. Mutating `request` is forwarded upstream;
/// headers are attached to the final response.
pub struct RequestContext<'a> {
    pub meta: Value,
    pub headers: &'a mut Vec<(String, String)>,
    pub request: &'a mut Value,
    pub state: &'a mut Value,
}

impl RequestContext<'_> {
    pub fn request_model(&self) -> Option<&str> {
        self.request.get("model").and_then(Value::as_str)
    }
}

/// Per-attempt provider request, after routing selected a target.
pub struct ProviderRequestContext<'a> {
    pub meta: Value,
    pub provider: Value,
    pub request: &'a mut Value,
    pub state: &'a mut Value,
}

pub struct ResponseContext<'a> {
    pub meta: Value,
    pub response: &'a mut Value,
    pub usage: &'a mut Value,
    pub state: &'a mut Value,
}

pub struct StreamContext<'a> {
    pub meta: Value,
    pub event: &'a mut Value,
    pub state: &'a mut Value,
}

pub struct ErrorContext<'a> {
    pub meta: Value,
    pub provider_id: Option<String>,
    pub account_id: Option<String>,
    pub error: &'a mut Value,
    pub state: &'a mut Value,
}

/// Content-free log event. The state is readable but the host ignores mutations
/// at this stage.
pub struct LogContext<'a> {
    pub meta: Value,
    pub event: Value,
    pub state: &'a mut Value,
}

/// Lifecycle implemented by a plugin. Every stage defaults to continuing
/// unchanged.
pub trait LlmPlugin: Sized {
    fn configure(config: Value) -> Result<Self>;

    fn on_request(&mut self, _cx: &mut RequestContext<'_>) -> Result<Decision> {
        Ok(Decision::Continue)
    }

    fn on_provider_request(&mut self, _cx: &mut ProviderRequestContext<'_>) -> Result<Decision> {
        Ok(Decision::Continue)
    }

    fn on_response(&mut self, _cx: &mut ResponseContext<'_>) -> Result<Decision> {
        Ok(Decision::Continue)
    }

    fn on_stream_event(&mut self, _cx: &mut StreamContext<'_>) -> Result<StreamDecision> {
        Ok(StreamDecision::Continue)
    }

    fn on_error(&mut self, _cx: &mut ErrorContext<'_>) -> Result<Decision> {
        Ok(Decision::Continue)
    }

    fn on_log(&mut self, _cx: &mut LogContext<'_>) {}
}

/// Exports `$plugin` as the component's `plugin-api` implementation.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty, $bindings:ident) => {
        static __OCTRA_LLM_PLUGIN_STATE: ::std::sync::Mutex<::std::option::Option<$plugin>> =
            ::std::sync::Mutex::new(::std::option::Option::None);

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct __OctraLlmPluginExport;

        fn __octra_parse_json(
            raw: &str,
            what: &str,
        ) -> ::std::result::Result<$crate::__Value, ::std::string::String> {
            $crate::__serde_json::from_str(raw)
                .map_err(|error| ::std::format!("invalid {what} JSON: {error}"))
        }

        fn __octra_parse_state(
            raw: &str,
        ) -> ::std::result::Result<$crate::__Value, ::std::string::String> {
            if raw.is_empty() || raw == "null" {
                ::std::result::Result::Ok($crate::__Value::Null)
            } else {
                __octra_parse_json(raw, "state")
            }
        }

        fn __octra_outcome(
            action: &str,
            payload: ::std::option::Option<$crate::__Value>,
            state: &$crate::__Value,
            error: ::std::option::Option<$crate::GatewayError>,
            headers: ::std::vec::Vec<(::std::string::String, ::std::string::String)>,
        ) -> $bindings::exports::octra::llm_plugin::plugin_api::Outcome {
            $bindings::exports::octra::llm_plugin::plugin_api::Outcome {
                action: action.to_string(),
                payload_json: payload.map(|value| value.to_string()),
                state_json: state.to_string(),
                error_json: error.map(|error| error.to_json().to_string()),
                headers,
            }
        }

        impl $bindings::exports::octra::llm_plugin::plugin_api::Guest for __OctraLlmPluginExport {
            fn configure(
                config_json: ::std::string::String,
            ) -> ::std::result::Result<(), ::std::string::String> {
                let config = __octra_parse_json(&config_json, "config")?;
                let plugin = <$plugin as $crate::LlmPlugin>::configure(config)
                    .map_err(|error| error.to_string())?;
                let mut state = __OCTRA_LLM_PLUGIN_STATE
                    .lock()
                    .map_err(|_| "plugin state lock poisoned".to_string())?;
                *state = ::std::option::Option::Some(plugin);
                ::std::result::Result::Ok(())
            }

            fn on_stage(
                call: $bindings::exports::octra::llm_plugin::plugin_api::Invocation,
            ) -> ::std::result::Result<
                $bindings::exports::octra::llm_plugin::plugin_api::Outcome,
                ::std::string::String,
            > {
                use $bindings::octra::llm_plugin::types::Stage;

                let meta = __octra_parse_json(&call.meta_json, "metadata")?;
                let mut state = __octra_parse_state(&call.state_json)?;
                let mut headers: ::std::vec::Vec<(
                    ::std::string::String,
                    ::std::string::String,
                )> = ::std::vec::Vec::new();
                let mut guard = __OCTRA_LLM_PLUGIN_STATE
                    .lock()
                    .map_err(|_| "plugin state lock poisoned".to_string())?;
                let plugin = guard
                    .as_mut()
                    .ok_or_else(|| "plugin is not configured".to_string())?;

                match call.stage {
                    Stage::Request => {
                        let payload = __octra_parse_json(&call.payload_json, "request payload")?;
                        let mut request = payload
                            .get("request")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let decision = {
                            let mut cx = $crate::RequestContext {
                                meta,
                                headers: &mut headers,
                                request: &mut request,
                                state: &mut state,
                            };
                            <$plugin as $crate::LlmPlugin>::on_request(plugin, &mut cx)?
                        };
                        match decision {
                            $crate::Decision::Continue => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "continue",
                                    ::std::option::Option::Some(
                                        $crate::__serde_json::json!({ "request": request }),
                                    ),
                                    &state,
                                    ::std::option::Option::None,
                                    headers,
                                ))
                            }
                            $crate::Decision::Reject(error) => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "reject",
                                    ::std::option::Option::None,
                                    &state,
                                    ::std::option::Option::Some(error),
                                    ::std::vec::Vec::new(),
                                ))
                            }
                        }
                    }
                    Stage::ProviderRequest => {
                        let payload = __octra_parse_json(&call.payload_json, "provider payload")?;
                        let mut request = payload
                            .get("request")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let provider = payload
                            .get("provider")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let decision = {
                            let mut cx = $crate::ProviderRequestContext {
                                meta,
                                provider,
                                request: &mut request,
                                state: &mut state,
                            };
                            <$plugin as $crate::LlmPlugin>::on_provider_request(plugin, &mut cx)?
                        };
                        match decision {
                            $crate::Decision::Continue => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "continue",
                                    ::std::option::Option::Some(
                                        $crate::__serde_json::json!({ "request": request }),
                                    ),
                                    &state,
                                    ::std::option::Option::None,
                                    headers,
                                ))
                            }
                            $crate::Decision::Reject(error) => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "reject",
                                    ::std::option::Option::None,
                                    &state,
                                    ::std::option::Option::Some(error),
                                    ::std::vec::Vec::new(),
                                ))
                            }
                        }
                    }
                    Stage::Response => {
                        let payload = __octra_parse_json(&call.payload_json, "response payload")?;
                        let mut response = payload
                            .get("response")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let mut usage = payload
                            .get("usage")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let decision = {
                            let mut cx = $crate::ResponseContext {
                                meta,
                                response: &mut response,
                                usage: &mut usage,
                                state: &mut state,
                            };
                            <$plugin as $crate::LlmPlugin>::on_response(plugin, &mut cx)?
                        };
                        match decision {
                            $crate::Decision::Continue => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "continue",
                                    ::std::option::Option::Some($crate::__serde_json::json!({
                                        "response": response,
                                        "usage": usage,
                                    })),
                                    &state,
                                    ::std::option::Option::None,
                                    headers,
                                ))
                            }
                            $crate::Decision::Reject(error) => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "reject",
                                    ::std::option::Option::None,
                                    &state,
                                    ::std::option::Option::Some(error),
                                    ::std::vec::Vec::new(),
                                ))
                            }
                        }
                    }
                    Stage::StreamEvent => {
                        let payload = __octra_parse_json(&call.payload_json, "stream payload")?;
                        let mut event = payload
                            .get("event")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let decision = {
                            let mut cx = $crate::StreamContext {
                                meta,
                                event: &mut event,
                                state: &mut state,
                            };
                            <$plugin as $crate::LlmPlugin>::on_stream_event(plugin, &mut cx)?
                        };
                        match decision {
                            $crate::StreamDecision::Continue => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "continue",
                                    ::std::option::Option::Some(
                                        $crate::__serde_json::json!({ "event": event }),
                                    ),
                                    &state,
                                    ::std::option::Option::None,
                                    headers,
                                ))
                            }
                            $crate::StreamDecision::Replace(events) => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "replace",
                                    ::std::option::Option::Some(
                                        $crate::__serde_json::json!({ "events": events }),
                                    ),
                                    &state,
                                    ::std::option::Option::None,
                                    headers,
                                ))
                            }
                            $crate::StreamDecision::Reject(error) => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "reject",
                                    ::std::option::Option::None,
                                    &state,
                                    ::std::option::Option::Some(error),
                                    ::std::vec::Vec::new(),
                                ))
                            }
                        }
                    }
                    Stage::Error => {
                        let payload = __octra_parse_json(&call.payload_json, "error payload")?;
                        let mut error = payload
                            .get("error")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let provider_id = payload
                            .get("provider_id")
                            .and_then($crate::__Value::as_str)
                            .map(::std::string::ToString::to_string);
                        let account_id = payload
                            .get("account_id")
                            .and_then($crate::__Value::as_str)
                            .map(::std::string::ToString::to_string);
                        let decision = {
                            let mut cx = $crate::ErrorContext {
                                meta,
                                provider_id,
                                account_id,
                                error: &mut error,
                                state: &mut state,
                            };
                            <$plugin as $crate::LlmPlugin>::on_error(plugin, &mut cx)?
                        };
                        match decision {
                            $crate::Decision::Continue => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "continue",
                                    ::std::option::Option::Some(
                                        $crate::__serde_json::json!({ "error": error }),
                                    ),
                                    &state,
                                    ::std::option::Option::None,
                                    headers,
                                ))
                            }
                            $crate::Decision::Reject(error) => {
                                ::std::result::Result::Ok(__octra_outcome(
                                    "reject",
                                    ::std::option::Option::None,
                                    &state,
                                    ::std::option::Option::Some(error),
                                    ::std::vec::Vec::new(),
                                ))
                            }
                        }
                    }
                    Stage::Log => {
                        let event = __octra_parse_json(&call.payload_json, "log payload")?
                            .get("event")
                            .cloned()
                            .unwrap_or($crate::__Value::Null);
                        let mut cx = $crate::LogContext {
                            meta,
                            event,
                            state: &mut state,
                        };
                        <$plugin as $crate::LlmPlugin>::on_log(plugin, &mut cx);
                        ::std::result::Result::Ok(__octra_outcome(
                            "continue",
                            ::std::option::Option::None,
                            &state,
                            ::std::option::Option::None,
                            headers,
                        ))
                    }
                }
            }
        }

        $bindings::export!(__OctraLlmPluginExport with_types_in $bindings);
    };
}
