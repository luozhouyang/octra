//! JSON-RPC 2.0 envelope types shared by every MCP transport.
//!
//! Responses are accepted from heterogeneous upstreams, so notification and
//! error shapes are lenient in what they deserialize and strict in what Octra
//! itself emits.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Standard JSON-RPC 2.0 error codes plus the MCP transport extension codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    /// The tool namespace is unknown or the tool does not exist.
    pub const TOOL_NOT_FOUND: i64 = -32002;
    /// The caller (consumer) is not allowed to invoke the tool.
    pub const FORBIDDEN: i64 = -32005;
    /// The upstream MCP server failed or timed out.
    pub const UPSTREAM_ERROR: i64 = -32006;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: Value, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
            id: Some(id),
        }
    }

    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
            id: None,
        }
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn parse_error() -> Self {
        Self::new(codes::PARSE_ERROR, "parse error")
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_REQUEST, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL_ERROR, message)
    }
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for JsonRpcError {}

/// Server-to-client requests (sampling, elicitation, roots) use the same
/// envelope as [`JsonRpcRequest`] but a disjoint method namespace.
pub type JsonRpcServerRequest = JsonRpcRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    pub id: Value,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn failure(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }

    /// Returns the result or the error, never both.
    pub fn into_result(self) -> Result<Value, JsonRpcError> {
        match (self.result, self.error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(error),
            (None, None) => Err(JsonRpcError::invalid_request(
                "response carries neither result nor error",
            )),
        }
    }
}

fn is_request(object: &Value) -> bool {
    object.get("method").is_some()
        && object.get("result").is_none()
        && object.get("error").is_none()
}

fn is_response(object: &Value) -> bool {
    object.get("id").is_some() && (object.get("result").is_some() || object.get("error").is_some())
}

/// A decoded inbound message from an upstream or client.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
}

impl JsonRpcMessage {
    /// Classifies one JSON value (a single message or one entry of a batch).
    pub fn classify(value: Value) -> Result<Self, JsonRpcError> {
        let Value::Object(object) = value else {
            return Err(JsonRpcError::invalid_request(
                "JSON-RPC message must be an object",
            ));
        };

        let value = Value::Object(object);

        if is_request(&value) {
            let request = serde_json::from_value(value)
                .map_err(|err| JsonRpcError::invalid_request(err.to_string()))?;
            return Ok(Self::Request(request));
        }

        if is_response(&value) {
            let response = serde_json::from_value(value)
                .map_err(|err| JsonRpcError::invalid_request(err.to_string()))?;
            return Ok(Self::Response(response));
        }

        Err(JsonRpcError::invalid_request(
            "message is neither a request nor a response",
        ))
    }

    /// Classifies a stream frame: a single message or a newline-delimited
    /// run of messages. Invalid frames are rejected; transports skip them.
    pub fn parse_stream_frame(text: &str) -> Vec<Self> {
        let mut messages = Vec::new();
        if let Ok(items) = serde_json::from_str::<Vec<Value>>(text) {
            messages.extend(
                items
                    .into_iter()
                    .filter_map(|value| Self::classify(value).ok()),
            );
            return messages;
        }
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            if let Ok(message) = Self::classify(value) {
                messages.push(message);
            }
            return messages;
        }
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if let Ok(value) = serde_json::from_str::<Value>(line)
                && let Ok(message) = Self::classify(value)
            {
                messages.push(message);
            }
        }
        messages
    }

    /// Parses a JSON payload that may hold a batch.
    pub fn parse_batch(text: &str) -> Result<Vec<Self>, JsonRpcError> {
        let value: Value = serde_json::from_str(text).map_err(|_| JsonRpcError::parse_error())?;
        match value {
            Value::Array(items) => items.into_iter().map(Self::classify).collect(),
            other => Ok(vec![Self::classify(other)?]),
        }
    }
}

/// Builds a notification envelope.
pub fn notification(method: &str, params: Option<Value>) -> Value {
    let mut object = Map::new();
    object.insert("jsonrpc".into(), json!("2.0"));
    object.insert("method".into(), json!(method));
    if let Some(params) = params {
        object.insert("params".into(), params);
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_requests_and_notifications() {
        let request =
            JsonRpcMessage::parse_batch(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#)
                .unwrap();
        assert!(matches!(&request[0], JsonRpcMessage::Request(r) if !r.is_notification()));

        let note = JsonRpcMessage::parse_batch(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .unwrap();
        assert!(matches!(&note[0], JsonRpcMessage::Request(r) if r.is_notification()));
    }

    #[test]
    fn classifies_responses_in_batches() {
        let messages = JsonRpcMessage::parse_batch(
            r#"[{"jsonrpc":"2.0","id":"a","result":{"ok":true}},{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}]"#,
        )
        .unwrap();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], JsonRpcMessage::Response(r) if r.error.is_none()));
        assert!(matches!(&messages[1], JsonRpcMessage::Response(r) if r.error.is_some()));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            JsonRpcMessage::parse_batch("not json").unwrap_err().code,
            codes::PARSE_ERROR
        );
        assert_eq!(
            JsonRpcMessage::parse_batch(r#"{"foo":1}"#)
                .unwrap_err()
                .code,
            codes::INVALID_REQUEST
        );
    }

    #[test]
    fn stream_frames_may_hold_several_messages() {
        let messages = JsonRpcMessage::parse_stream_frame(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n",
        );
        assert_eq!(messages.len(), 2);
        assert!(JsonRpcMessage::parse_stream_frame("noise").is_empty());
    }

    #[test]
    fn response_into_result_prefers_result() {
        let response = JsonRpcResponse::success(json!(1), json!({"k":"v"}));
        assert_eq!(response.into_result().unwrap(), json!({"k":"v"}));

        let response = JsonRpcResponse::failure(json!(1), JsonRpcError::method_not_found("x"));
        assert_eq!(
            response.into_result().unwrap_err().code,
            codes::METHOD_NOT_FOUND
        );
    }
}
