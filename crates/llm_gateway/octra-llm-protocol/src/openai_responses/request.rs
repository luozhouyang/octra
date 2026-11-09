//! OpenAI Responses requests to/from canonical IR.
//!
//! The Responses API is item-oriented: `input` is a list of messages, function
//! calls, function outputs and reasoning items. Stateful fields that the IR
//! cannot model (`previous_response_id`, `store`, `metadata`) survive in
//! `extra` for same-protocol forwarding.

use octra_llm_core::{
    AssistantContent, Attachment, AttachmentData, ChatRequest, ContentPart, FunctionDefinition,
    Map, Message, ProtocolError, ProtocolResult, Role, SystemPrompt, Tool, ToolCallKind,
    ToolResult, UserContent, Value,
};
use serde_json::json;

const KNOWN_FIELDS: [&str; 18] = [
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "max_output_tokens",
    "stream",
    "store",
    "previous_response_id",
    "metadata",
    "text",
    "reasoning",
    "truncation",
    "include",
    "user",
];

pub fn decode_request(value: Value) -> ProtocolResult<ChatRequest> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("request body must be a JSON object"))?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("missing string field `model`"))?
        .to_string();

    let mut system = SystemPrompt::default();
    if let Some(instructions) = object.get("instructions").and_then(Value::as_str)
        && !instructions.is_empty()
    {
        system.blocks.push(instructions.to_string());
    }

    let mut messages = Vec::new();
    match object.get("input") {
        Some(Value::String(text)) => messages.push(Message::user(text.clone())),
        Some(Value::Array(items)) => decode_items(items, &mut messages, &mut system)?,
        Some(_) => {
            return Err(ProtocolError::new(
                "`input` must be a string or an array of items",
            ));
        }
        None => {
            if object.get("previous_response_id").is_none() {
                return Err(ProtocolError::new(
                    "missing `input` and `previous_response_id`",
                ));
            }
        }
    }
    if messages.is_empty() && object.get("previous_response_id").is_none() {
        return Err(ProtocolError::new("`input` must contain at least one item"));
    }

    let mut request = ChatRequest::new(model, messages);
    request.system = system;
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        request.tools = tools.iter().filter_map(decode_tool).collect();
    }
    request.tool_choice = object.get("tool_choice").cloned();
    request.parallel_tool_calls = object.get("parallel_tool_calls").and_then(Value::as_bool);
    request.temperature = object.get("temperature").and_then(Value::as_f64);
    request.top_p = object.get("top_p").and_then(Value::as_f64);
    request.max_tokens = object
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .map(|value| value.min(u32::MAX as u64) as u32);
    request.stream = object.get("stream").and_then(Value::as_bool);
    request.reasoning_effort = object
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    request.user = object
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_string);

    for (key, value) in object {
        if !KNOWN_FIELDS.contains(&key.as_str()) {
            request.extra.insert(key.clone(), value.clone());
        }
    }
    // Stateful fields must survive for same-protocol forwarding.
    for key in [
        "previous_response_id",
        "store",
        "metadata",
        "text",
        "truncation",
        "include",
    ] {
        if let Some(value) = object.get(key) {
            request.extra.insert(key.to_string(), value.clone());
        }
    }
    Ok(request)
}

fn decode_items(
    items: &[Value],
    messages: &mut Vec<Message>,
    system: &mut SystemPrompt,
) -> ProtocolResult<()> {
    for item in items {
        let object = item
            .as_object()
            .ok_or_else(|| ProtocolError::new("input items must be objects"))?;
        match object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
        {
            "message" => decode_message(object, messages, system)?,
            "function_call" => {
                let id = object
                    .get("call_id")
                    .or_else(|| object.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_missing")
                    .to_string();
                let name = object
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = object
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .unwrap_or_else(|| json!({}));
                push_assistant(
                    messages,
                    AssistantContent::ToolCall {
                        id,
                        name,
                        arguments,
                        kind: Some(ToolCallKind::Function),
                    },
                );
            }
            "function_call_output" => {
                let call_id = object
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_missing")
                    .to_string();
                let output = match object.get("output") {
                    Some(Value::String(text)) => UserContent::text(text.clone()),
                    Some(Value::Array(parts)) => {
                        UserContent::from_parts(parts.iter().map(decode_output_part).collect())
                    }
                    Some(other) => UserContent::text(other.to_string()),
                    None => UserContent::text(String::new()),
                };
                messages.push(Message {
                    role: Role::Tool,
                    content: None,
                    assistant: Vec::new(),
                    tool_results: vec![ToolResult {
                        tool_call_id: call_id,
                        name: None,
                        content: output,
                        is_error: false,
                    }],
                    name: None,
                    extra: Map::new(),
                });
            }
            "reasoning" => {
                let text = object
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|summary| {
                        summary
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    push_assistant(
                        messages,
                        AssistantContent::Reasoning {
                            text,
                            signature: object
                                .get("encrypted_content")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        },
                    );
                }
            }
            other => {
                push_assistant(
                    messages,
                    AssistantContent::Raw {
                        kind: other.to_string(),
                        raw: item.clone(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn push_assistant(messages: &mut Vec<Message>, block: AssistantContent) {
    if let Some(message) = messages.last_mut()
        && message.role == Role::Assistant
    {
        message.assistant.push(block);
        return;
    }
    let mut message = Message::new(Role::Assistant);
    message.assistant.push(block);
    messages.push(message);
}

fn decode_message(
    object: &Map<String, Value>,
    messages: &mut Vec<Message>,
    system: &mut SystemPrompt,
) -> ProtocolResult<()> {
    let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
    let content = object.get("content");
    if matches!(role, "system" | "developer") {
        for text in text_parts(content) {
            system.blocks.push(text);
        }
        return Ok(());
    }
    if role == "assistant" {
        let mut message = Message::new(Role::Assistant);
        for text in text_parts(content) {
            if !text.is_empty() {
                message.assistant.push(AssistantContent::Text { text });
            }
        }
        messages.push(message);
        return Ok(());
    }
    messages.push(Message::user_parts(decode_content(content)));
    Ok(())
}

fn text_parts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_content(content: Option<&Value>) -> Vec<ContentPart> {
    match content {
        Some(Value::String(text)) => vec![ContentPart::text(text.clone())],
        Some(Value::Array(parts)) => parts.iter().map(decode_output_part).collect(),
        _ => Vec::new(),
    }
}

fn decode_output_part(part: &Value) -> ContentPart {
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("input_text");
    match kind {
        "input_text" | "output_text" | "text" | "summary_text" => ContentPart::Text {
            text: part
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            extra: Map::new(),
        },
        "input_image" => {
            let url = part
                .get("image_url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            decode_image(url)
        }
        "input_file" => {
            let media_type = part
                .get("mime_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            let data =
                part.get("file_data")
                    .and_then(Value::as_str)
                    .map(|value| AttachmentData::Base64 {
                        data: value.to_string(),
                    });
            ContentPart::File(Attachment {
                media_type,
                data,
                url: part
                    .get("file_url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: part
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                extra: Map::new(),
            })
        }
        other => ContentPart::Unsupported {
            kind: other.to_string(),
            raw: part.clone(),
        },
    }
}

fn decode_image(url: &str) -> ContentPart {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((meta, data)) = rest.split_once(',')
    {
        let media_type = meta
            .split(';')
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or("image/png")
            .to_string();
        return ContentPart::Image(Attachment {
            media_type,
            data: Some(AttachmentData::Base64 {
                data: data.to_string(),
            }),
            url: None,
            name: None,
            extra: Map::new(),
        });
    }
    ContentPart::Image(Attachment {
        media_type: "image/png".to_string(),
        data: None,
        url: (!url.is_empty()).then(|| url.to_string()),
        name: None,
        extra: Map::new(),
    })
}

fn decode_tool(tool: &Value) -> Option<Tool> {
    match tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function")
    {
        "function" => Some(Tool::function(FunctionDefinition {
            name: tool.get("name")?.as_str()?.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters: tool.get("parameters").cloned(),
            strict: tool.get("strict").and_then(Value::as_bool),
        })),
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
    if !request.system.is_empty() {
        object.insert(
            "instructions".to_string(),
            json!(request.system.blocks.join("\n\n")),
        );
    }

    let mut input = Vec::new();
    for message in &request.messages {
        encode_message(message, &mut input);
    }
    object.insert("input".to_string(), Value::Array(input));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .filter_map(|tool| match tool.kind {
                ToolCallKind::Function => tool.function.as_ref().map(|function| {
                    let mut spec = Map::new();
                    spec.insert("type".to_string(), json!("function"));
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
                }),
                ToolCallKind::Provider => tool.provider_config.clone(),
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
        object.insert("max_output_tokens".to_string(), json!(max_tokens));
    }
    if let Some(stream) = request.stream {
        object.insert("stream".to_string(), json!(stream));
    }
    if let Some(effort) = &request.reasoning_effort {
        object.insert("reasoning".to_string(), json!({"effort": effort}));
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
    if choice.get("type").and_then(Value::as_str) == Some("function")
        && choice.get("name").is_none()
        && let Some(name) = choice.get("function").and_then(|f| f.get("name"))
    {
        return json!({"type": "function", "name": name});
    }
    choice.clone()
}

fn encode_message(message: &Message, out: &mut Vec<Value>) {
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
            out.push(json!({
                "type": "message",
                "role": if message.role == Role::Developer { "developer" } else { "system" },
                "content": [{"type": "input_text", "text": text}],
            }));
        }
        Role::User => out.push(json!({
            "type": "message",
            "role": "user",
            "content": encode_content(
                message
                    .content
                    .as_ref()
                    .map(|content| content.parts.as_slice())
                    .unwrap_or(&[])
            ),
        })),
        Role::Assistant => {
            for block in &message.assistant {
                match block {
                    AssistantContent::Text { text } => out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    })),
                    AssistantContent::Refusal { text } => out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "refusal", "refusal": text}],
                    })),
                    AssistantContent::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => out.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments.to_string(),
                    })),
                    AssistantContent::Reasoning { text, .. } => out.push(json!({
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": text}],
                    })),
                    AssistantContent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => out.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": if *is_error { format!("error: {content}") } else { content.to_string() },
                    })),
                    AssistantContent::Raw { kind, raw } => {
                        if kind.starts_with("openai_responses") {
                            out.push(raw.clone());
                        }
                    }
                }
            }
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
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": result.tool_call_id,
                    "output": text,
                }));
            }
        }
    }
}

fn encode_content(parts: &[ContentPart]) -> Value {
    let mut encoded = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text, .. } => {
                encoded.push(json!({"type": "input_text", "text": text}));
            }
            ContentPart::Image(image) => {
                let url = match (&image.data, &image.url) {
                    (Some(AttachmentData::Base64 { data }), _) => {
                        format!("data:{};base64,{data}", image.media_type)
                    }
                    (_, Some(url)) => url.clone(),
                    _ => continue,
                };
                let mut spec = json!({"type": "input_image", "image_url": url});
                if let Some(detail) = image.extra.get("detail") {
                    spec["detail"] = detail.clone();
                }
                encoded.push(spec);
            }
            ContentPart::File(file) => {
                let mut spec = json!({"type": "input_file"});
                if let Some(name) = &file.name {
                    spec["filename"] = json!(name);
                }
                match &file.data {
                    Some(AttachmentData::Base64 { data }) => {
                        spec["file_data"] = json!(data);
                        spec["mime_type"] = json!(file.media_type);
                    }
                    None => {
                        if let Some(url) = &file.url {
                            spec["file_url"] = json!(url);
                        }
                    }
                }
                encoded.push(spec);
            }
            ContentPart::Reasoning { text, .. } => encoded.push(json!({
                "type": "input_text",
                "text": text,
            })),
            ContentPart::Unsupported { raw, .. } => encoded.push(raw.clone()),
        }
    }
    Value::Array(encoded)
}
