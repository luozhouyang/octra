//! OpenAI Chat Completions request to/from canonical IR.

use octra_llm_core::{
    AssistantContent, Attachment, AttachmentData, ChatRequest, ContentPart, FunctionDefinition,
    Map, Message, ProtocolError, ProtocolResult, Role, SystemPrompt, Tool, ToolCallKind,
    UserContent, Value,
};
use serde_json::json;

const KNOWN_FIELDS: [&str; 17] = [
    "model",
    "messages",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "stream",
    "stream_options",
    "response_format",
    "reasoning_effort",
    "user",
    "functions",
    "function_call",
];

pub fn decode_request(value: Value) -> ProtocolResult<ChatRequest> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("request body must be a JSON object"))?;

    if object.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        return Err(ProtocolError::new(
            "OpenAI `n` greater than 1 is not supported by this gateway",
        ));
    }

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("missing string field `model`"))?
        .to_string();

    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("missing array field `messages`"))?;

    let mut system = SystemPrompt::default();
    let mut converted = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(message) = decode_message(message, &mut system)? {
            converted.push(message);
        }
    }
    if converted.is_empty() {
        return Err(ProtocolError::new(
            "`messages` must contain at least one message",
        ));
    }

    let mut request = ChatRequest::new(model, convert_json_strings(converted));
    request.system = system;

    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        request.tools = tools.iter().filter_map(decode_tool).collect();
    }
    request.tool_choice = object.get("tool_choice").cloned();
    request.parallel_tool_calls = object.get("parallel_tool_calls").and_then(Value::as_bool);
    request.temperature = object.get("temperature").and_then(Value::as_f64);
    request.top_p = object.get("top_p").and_then(Value::as_f64);
    request.max_tokens = object
        .get("max_completion_tokens")
        .or_else(|| object.get("max_tokens"))
        .and_then(Value::as_u64)
        .map(|value| value.min(u32::MAX as u64) as u32);
    if object.get("max_completion_tokens").is_some() {
        // OpenAI reasoning models reject `max_tokens`; remember which spelling
        // the client used so the encoder can preserve it.
        request
            .extra
            .insert("openai_max_completion_tokens".into(), Value::Bool(true));
    }
    request.stop = match object.get("stop") {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    request.stream = object.get("stream").and_then(Value::as_bool);
    request.include_usage = object
        .get("stream_options")
        .and_then(|options| options.get("include_usage"))
        .and_then(Value::as_bool);
    request.response_format = object.get("response_format").cloned();
    request.reasoning_effort = object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    request.user = object
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_string);

    for (key, value) in object {
        if !KNOWN_FIELDS.contains(&key.as_str()) && key != "model" && key != "messages" {
            request.extra.insert(key.clone(), value.clone());
        }
    }

    Ok(request)
}

fn convert_json_strings(messages: Vec<ConvertedMessage>) -> Vec<Message> {
    messages.into_iter().map(|message| message.0).collect()
}

struct ConvertedMessage(Message);

fn decode_message(
    value: &Value,
    system: &mut SystemPrompt,
) -> ProtocolResult<Option<ConvertedMessage>> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("each message must be an object"))?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("each message needs a string `role`"))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut extra = Map::new();
    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "role"
                | "content"
                | "name"
                | "tool_calls"
                | "tool_call_id"
                | "function_call"
                | "refusal"
        ) {
            extra.insert(key.clone(), value.clone());
        }
    }

    match role {
        "system" | "developer" => {
            for text in content_texts(object.get("content")) {
                system.blocks.push(text);
            }
            Ok(None)
        }
        "user" => Ok(Some(ConvertedMessage(Message {
            role: Role::User,
            content: Some(decode_user_content(object.get("content"))?),
            assistant: Vec::new(),
            tool_results: Vec::new(),
            name,
            extra,
        }))),
        "assistant" => {
            let mut message = Message {
                role: Role::Assistant,
                content: None,
                assistant: Vec::new(),
                tool_results: Vec::new(),
                name,
                extra,
            };
            let text = content_texts(object.get("content")).join("");
            if let Some(reasoning) = object
                .get("reasoning_content")
                .or_else(|| object.get("reasoning"))
                .and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                message.assistant.push(AssistantContent::Reasoning {
                    text: reasoning.to_string(),
                    signature: None,
                });
            }
            if !text.is_empty() {
                message.assistant.push(AssistantContent::Text { text });
            }
            if let Some(Value::String(refusal)) = object.get("refusal") {
                message.assistant.push(AssistantContent::Refusal {
                    text: refusal.clone(),
                });
            }
            if let Some(calls) = object.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    message.assistant.push(decode_tool_call(call));
                }
            }
            if let Some(call) = object.get("function_call") {
                message.assistant.push(decode_function_call_block(call));
            }
            Ok(Some(ConvertedMessage(message)))
        }
        "tool" => {
            let tool_call_id = object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProtocolError::new("tool messages need `tool_call_id`"))?
                .to_string();
            Ok(Some(ConvertedMessage(Message {
                role: Role::Tool,
                content: None,
                assistant: Vec::new(),
                tool_results: vec![octra_llm_core::ToolResult {
                    tool_call_id,
                    name,
                    content: UserContent::text(content_texts(object.get("content")).join("")),
                    is_error: false,
                }],
                name: None,
                extra,
            })))
        }
        "function" => Ok(Some(ConvertedMessage(Message {
            role: Role::Tool,
            content: None,
            assistant: Vec::new(),
            tool_results: vec![octra_llm_core::ToolResult {
                tool_call_id: object
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("legacy_function")
                    .to_string(),
                name,
                content: UserContent::text(content_texts(object.get("content")).join("")),
                is_error: false,
            }],
            name: None,
            extra,
        }))),
        other => Err(ProtocolError::new(format!(
            "unsupported message role `{other}`"
        ))),
    }
}

/// Extracts text from a string, an array of text parts, or `null`.
fn content_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => map
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|_| map.get("type").and_then(Value::as_str) != Some("image_url"))
                    .map(str::to_string),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_user_content(content: Option<&Value>) -> ProtocolResult<UserContent> {
    match content {
        Some(Value::String(text)) => Ok(UserContent::text(text.clone())),
        Some(Value::Array(parts)) => {
            let mut converted = Vec::with_capacity(parts.len());
            for part in parts {
                converted.push(decode_content_part(part)?);
            }
            Ok(UserContent::from_parts(converted))
        }
        None => Ok(UserContent::text("")),
        _ => Err(ProtocolError::new(
            "message content must be a string or an array",
        )),
    }
}

fn decode_content_part(part: &Value) -> ProtocolResult<ContentPart> {
    let object = part
        .as_object()
        .ok_or_else(|| ProtocolError::new("content parts must be objects"))?;
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("text");
    match kind {
        "text" | "input_text" | "output_text" => Ok(ContentPart::Text {
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            extra: Map::new(),
        }),
        "image_url" => {
            let mut url = object
                .get("image_url")
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let detail = object
                .get("image_url")
                .and_then(|value| value.get("detail"))
                .and_then(Value::as_str);
            let mut extra = Map::new();
            if let Some(detail) = detail {
                extra.insert("detail".to_string(), Value::String(detail.to_string()));
            }
            if let Some(data_url) = url.strip_prefix("data:")
                && let Some((meta, data)) = data_url.split_once(',')
            {
                let media_type = meta
                    .split(';')
                    .next()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("image/png")
                    .to_string();
                return Ok(ContentPart::Image(Attachment {
                    media_type,
                    data: Some(AttachmentData::Base64 {
                        data: data.to_string(),
                    }),
                    url: None,
                    name: object
                        .get("filename")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    extra,
                }));
            }
            Ok(ContentPart::Image(Attachment {
                media_type: "image/png".to_string(),
                data: None,
                url: Some(std::mem::take(&mut url)),
                name: None,
                extra,
            }))
        }
        "file" => {
            let file = object.get("file").cloned().unwrap_or(Value::Null);
            let media_type = file
                .get("mime_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            let data = file
                .get("file_data")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("data:"))
                .and_then(|value| value.split_once(','))
                .map(|(_, data)| AttachmentData::Base64 {
                    data: data.to_string(),
                });
            Ok(ContentPart::File(Attachment {
                media_type,
                data,
                url: file
                    .get("file_url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: file
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                extra: Map::new(),
            }))
        }
        "refusal" => Ok(ContentPart::Text {
            text: object
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            extra: Map::new(),
        }),
        other => Ok(ContentPart::Unsupported {
            kind: other.to_string(),
            raw: part.clone(),
        }),
    }
}

fn decode_tool_call(call: &Value) -> AssistantContent {
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("call_missing")
        .to_string();
    let function = call.get("function").cloned().unwrap_or(Value::Null);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (arguments, arguments_error) = decode_arguments(function.get("arguments"));
    AssistantContent::ToolCall {
        id,
        name,
        arguments: merge_arguments(arguments, arguments_error),

        kind: None,
    }
}

fn decode_function_call_block(call: &Value) -> AssistantContent {
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (arguments, arguments_error) = decode_arguments(call.get("arguments"));
    AssistantContent::ToolCall {
        id: format!("call_{name}"),
        name,
        arguments: merge_arguments(arguments, arguments_error),

        kind: None,
    }
}

/// Keeps arguments as the canonical `Value`. String arguments are parsed to
/// JSON when possible; unparsable strings are wrapped so the lossless
/// re-encoding can detect the raw text.
pub fn decode_arguments(value: Option<&Value>) -> (Value, Option<String>) {
    match value {
        Some(Value::String(text)) => {
            if text.trim().is_empty() {
                return (json!({}), None);
            }
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) => (parsed, None),
                Err(err) => (json!({ "__raw_arguments": text }), Some(err.to_string())),
            }
        }
        Some(other) => (other.clone(), None),
        None => (json!({}), None),
    }
}

fn merge_arguments(arguments: Value, _error: Option<String>) -> Value {
    arguments
}

fn decode_tool(tool: &Value) -> Option<Tool> {
    match tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function")
    {
        "function" => {
            let function = tool.get("function")?;
            Some(Tool::function(FunctionDefinition {
                name: function.get("name")?.as_str()?.to_string(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: function.get("parameters").cloned(),
                strict: function.get("strict").and_then(Value::as_bool),
            }))
        }
        other => Some(Tool {
            kind: ToolCallKind::Provider,
            name: tool.get("name").and_then(Value::as_str).map(str::to_string),
            function: None,
            provider_type: Some(other.to_string()),
            provider_config: Some(tool.clone()),
            extra: Map::new(),
        }),
    }
}

pub fn encode_request(request: &ChatRequest) -> ProtocolResult<Value> {
    let mut object = Map::new();
    object.insert("model".to_string(), json!(request.model));

    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": request.system.blocks.join("\n\n"),
        }));
    }
    for message in &request.messages {
        encode_message(message, &mut messages)?;
    }
    object.insert("messages".to_string(), Value::Array(messages));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .filter_map(|tool| match tool.kind {
                ToolCallKind::Function => tool.function.as_ref().map(|function| {
                    let mut spec = Map::new();
                    spec.insert("type".to_string(), json!("function"));
                    spec.insert("function".to_string(), encode_function(function));
                    Value::Object(spec)
                }),
                // OpenAI has no generic provider tools; custom ones are
                // forwarded verbatim, Anthropic built-ins are dropped.
                ToolCallKind::Provider => tool
                    .provider_config
                    .clone()
                    .filter(|config| config.get("type").and_then(Value::as_str) == Some("custom")),
            })
            .collect();
        if !tools.is_empty() {
            object.insert("tools".to_string(), Value::Array(tools));
        }
    }

    if let Some(choice) = &request.tool_choice {
        object.insert("tool_choice".to_string(), normalize_tool_choice(choice));
    }
    if let Some(parallel) = request.parallel_tool_calls {
        object.insert("parallel_tool_calls".to_string(), json!(parallel));
    }
    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        object.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(max_tokens) = request.max_tokens {
        let field = if request
            .extra
            .get("openai_max_completion_tokens")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        object.insert(field.to_string(), json!(max_tokens));
    }
    match request.stop.len() {
        0 => {}
        1 => {
            object.insert("stop".to_string(), json!(request.stop[0]));
        }
        _ => {
            object.insert("stop".to_string(), json!(request.stop));
        }
    }
    if let Some(stream) = request.stream {
        object.insert("stream".to_string(), json!(stream));
        if stream && request.include_usage.unwrap_or(false) {
            object.insert("stream_options".to_string(), json!({"include_usage": true}));
        }
    }
    if let Some(format) = &request.response_format {
        object.insert("response_format".to_string(), format.clone());
    }
    if let Some(effort) = &request.reasoning_effort {
        object.insert("reasoning_effort".to_string(), json!(effort));
    }
    if let Some(user) = &request.user {
        object.insert("user".to_string(), json!(user));
    }
    for (key, value) in &request.extra {
        object.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Ok(Value::Object(object))
}

fn normalize_tool_choice(choice: &Value) -> Value {
    if let Value::String(_) = choice {
        return choice.clone();
    }
    if choice.get("type").and_then(Value::as_str) == Some("function")
        && let Some(name) = choice.get("function").and_then(|f| f.get("name"))
    {
        return json!({"type": "function", "function": {"name": name}});
    }
    if choice.get("type").and_then(Value::as_str) == Some("tool")
        && let Some(name) = choice.get("name")
    {
        return json!({"type": "function", "function": {"name": name}});
    }
    choice.clone()
}

fn encode_function(function: &FunctionDefinition) -> Value {
    let mut spec = Map::new();
    spec.insert("name".to_string(), json!(function.name));
    if let Some(description) = &function.description {
        spec.insert("description".to_string(), json!(description));
    }
    if let Some(parameters) = &function.parameters {
        spec.insert("parameters".to_string(), parameters.clone());
    }
    if let Some(strict) = function.strict {
        spec.insert("strict".to_string(), json!(strict));
    }
    Value::Object(spec)
}

fn encode_message(message: &Message, out: &mut Vec<Value>) -> ProtocolResult<()> {
    match message.role {
        Role::System | Role::Developer => {
            let text = message
                .content
                .as_ref()
                .map(|content| {
                    content
                        .parts
                        .iter()
                        .filter_map(ContentPart::as_text)
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            let role = if message.role == Role::Developer {
                "developer"
            } else {
                "system"
            };
            out.push(json!({"role": role, "content": text}));
        }
        Role::User => {
            let mut object = Map::new();
            object.insert("role".to_string(), json!("user"));
            if let Some(name) = &message.name {
                object.insert("name".to_string(), json!(name));
            }
            let content = message
                .content
                .as_ref()
                .map(encode_user_content)
                .unwrap_or(Value::String(String::new()));
            object.insert("content".to_string(), content);
            out.push(Value::Object(object));
        }
        Role::Assistant => {
            let mut object = Map::new();
            object.insert("role".to_string(), json!("assistant"));
            let text = message
                .assistant
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let reasoning = message
                .assistant
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Reasoning { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            object.insert(
                "content".to_string(),
                if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                },
            );
            if let Some(refusal) = message.assistant.iter().find_map(|block| match block {
                AssistantContent::Refusal { text } => Some(text),
                _ => None,
            }) {
                object.insert("refusal".to_string(), json!(refusal));
            }
            if !reasoning.is_empty() {
                object.insert("reasoning_content".to_string(), json!(reasoning));
            }
            let mut calls = Vec::new();
            for block in &message.assistant {
                match block {
                    AssistantContent::ToolCall {
                        id,
                        name,
                        arguments,
                        kind: _,
                    } => calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": encode_arguments(arguments),
                        }
                    })),
                    AssistantContent::Raw { kind, raw } if kind == "openai_tool_call" => {
                        calls.push(raw.clone());
                    }
                    _ => {}
                }
            }
            if !calls.is_empty() {
                object.insert("tool_calls".to_string(), Value::Array(calls));
            }
            out.push(Value::Object(object));
        }
        Role::Tool if message.tool_results.is_empty() => {
            let mut object = Map::new();
            object.insert("role".to_string(), json!("user"));
            object.insert(
                "content".to_string(),
                message
                    .content
                    .as_ref()
                    .map(encode_user_content)
                    .unwrap_or_else(|| Value::String(String::new())),
            );
            out.push(Value::Object(object));
        }
        Role::Tool => {
            for result in &message.tool_results {
                let text = result
                    .content
                    .parts
                    .iter()
                    .filter_map(ContentPart::as_text)
                    .collect::<Vec<_>>()
                    .join("");
                let mut object = Map::new();
                object.insert("role".to_string(), json!("tool"));
                object.insert("tool_call_id".to_string(), json!(result.tool_call_id));
                if let Some(name) = &result.name {
                    object.insert("name".to_string(), json!(name));
                }
                object.insert("content".to_string(), json!(text));
                out.push(Value::Object(object));
            }
        }
    }
    Ok(())
}

pub fn encode_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::Object(map) if map.len() == 1 && map.contains_key("__raw_arguments") => {
            map.get("__raw_arguments").cloned().unwrap_or(json!("{}"))
        }
        Value::String(text) => json!(text),
        other => json!(other.to_string()),
    }
}

fn encode_user_content(content: &UserContent) -> Value {
    if let Some(text) = content.as_text() {
        return json!(text);
    }
    let mut parts = Vec::new();
    for part in &content.parts {
        match part {
            ContentPart::Text { text, .. } => parts.push(json!({"type": "text", "text": text})),
            ContentPart::Image(image) => {
                let url = match (&image.data, &image.url) {
                    (Some(AttachmentData::Base64 { data }), _) => {
                        format!("data:{};base64,{data}", image.media_type)
                    }
                    (_, Some(url)) => url.clone(),
                    _ => continue,
                };
                let mut image_url = Map::new();
                image_url.insert("url".to_string(), json!(url));
                if let Some(detail) = image.extra.get("detail") {
                    image_url.insert("detail".to_string(), detail.clone());
                }
                parts.push(json!({"type": "image_url", "image_url": image_url}));
            }
            ContentPart::File(file) => {
                let mut spec = Map::new();
                match &file.data {
                    Some(AttachmentData::Base64 { data }) => {
                        spec.insert(
                            "file_data".to_string(),
                            json!(format!("data:{};base64,{data}", file.media_type)),
                        );
                    }
                    None => {
                        if let Some(url) = &file.url {
                            spec.insert("file_url".to_string(), json!(url));
                        }
                    }
                }
                if let Some(name) = &file.name {
                    spec.insert("filename".to_string(), json!(name));
                }
                parts.push(json!({"type": "file", "file": spec}));
            }
            ContentPart::Reasoning { .. } | ContentPart::Unsupported { .. } => {}
        }
    }
    Value::Array(parts)
}
