//! Anthropic Messages request to/from canonical IR.

use octra_llm_core::{
    AssistantContent, Attachment, AttachmentData, ChatRequest, ContentPart, FunctionDefinition,
    Map, Message, ProtocolError, ProtocolResult, Role, SystemPrompt, Tool, ToolCallKind,
    UserContent, Value,
};
use serde_json::json;

use crate::common::decode_attachment;

const DEFAULT_MAX_TOKENS: u32 = 4096;

const KNOWN_FIELDS: [&str; 20] = [
    "model",
    "messages",
    "system",
    "max_tokens",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "stream",
    "tools",
    "tool_choice",
    "thinking",
    "metadata",
    "service_tier",
    "container",
    "mcp_servers",
    "output_format",
    "betas",
    "cache_control",
    "context_management",
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
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("missing array field `messages`"))?;

    let mut system = SystemPrompt::default();
    decode_system(object.get("system"), &mut system)?;

    let mut converted = Vec::with_capacity(messages.len());
    for message in messages {
        converted.push(decode_message(message)?);
    }
    if converted.is_empty() {
        return Err(ProtocolError::new(
            "`messages` must contain at least one message",
        ));
    }

    let mut request = ChatRequest::new(model, converted);
    request.system = system;
    request.max_tokens = Some(
        object
            .get("max_tokens")
            .and_then(Value::as_u64)
            .map(|value| value.min(u32::MAX as u64) as u32)
            .unwrap_or(DEFAULT_MAX_TOKENS),
    );
    request.temperature = object.get("temperature").and_then(Value::as_f64);
    request.top_p = object.get("top_p").and_then(Value::as_f64);
    request.top_k = object
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|value| value.min(u32::MAX as u64) as u32);
    request.stop = object
        .get("stop_sequences")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    request.stream = object.get("stream").and_then(Value::as_bool);
    request.include_usage = request.stream;

    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        request.tools = tools.iter().filter_map(decode_tool).collect();
    }
    if let Some(choice) = object.get("tool_choice") {
        request.tool_choice = decode_tool_choice(choice);
        if choice
            .get("disable_parallel_tool_use")
            .and_then(Value::as_bool)
            .is_some()
        {
            request.parallel_tool_calls = Some(
                !choice
                    .get("disable_parallel_tool_use")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
        }
    }
    if let Some(max) = object
        .get("output_format")
        .and_then(|format| format.get("json_schema"))
        .or_else(|| object.get("output_format"))
    {
        request.response_format = Some(json!({"type": "json_schema", "json_schema": max}));
    }
    if let Some(thinking) = object.get("thinking") {
        let effort = match thinking.get("type").and_then(Value::as_str) {
            Some("disabled") => None,
            Some("adaptive") => Some("medium".to_string()),
            _ => {
                let budget = thinking
                    .get("budget_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Some(
                    if budget >= 32_000 {
                        "high"
                    } else if budget >= 8_000 {
                        "medium"
                    } else {
                        "low"
                    }
                    .to_string(),
                )
            }
        };
        request.reasoning_effort = effort;
        request
            .extra
            .insert("anthropic_thinking".into(), thinking.clone());
    }

    for (key, value) in object {
        if KNOWN_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if !matches!(key.as_str(), "model" | "messages" | "system") {
            request.extra.insert(key.clone(), value.clone());
        }
    }
    Ok(request)
}

fn decode_system(value: Option<&Value>, system: &mut SystemPrompt) -> ProtocolResult<()> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(text)) => {
            system.blocks.push(text.clone());
            Ok(())
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block {
                    Value::String(text) => system.blocks.push(text.clone()),
                    Value::Object(map) => {
                        if map.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(text) = map.get("text").and_then(Value::as_str)
                        {
                            system.blocks.push(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Err(ProtocolError::new("`system` must be a string or an array")),
    }
}

fn decode_message(value: &Value) -> ProtocolResult<Message> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("each message must be an object"))?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("each message needs a string `role`"))?;
    let blocks = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("anthropic message content must be an array"))?;

    let mut extra = Map::new();
    for (key, value) in object {
        if !matches!(key.as_str(), "role" | "content") {
            extra.insert(key.clone(), value.clone());
        }
    }

    match role {
        "user" => {
            let mut parts = Vec::new();
            let mut tool_results = Vec::new();
            for block in blocks {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("text");
                match kind {
                    "text" => parts.push(ContentPart::Text {
                        text: block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        extra: extra_of(block, &["cache_control"]),
                    }),
                    "image" => parts.push(ContentPart::Image(decode_attachment(block, "image"))),
                    "document" => parts.push(ContentPart::File(decode_attachment(block, "file"))),
                    "tool_result" => {
                        tool_results.push(decode_tool_result(block)?);
                    }
                    other => parts.push(ContentPart::Unsupported {
                        kind: other.to_string(),
                        raw: block.clone(),
                    }),
                }
            }
            Ok(Message {
                role: Role::User,
                content: if parts.is_empty() {
                    None
                } else {
                    Some(UserContent::from_parts(parts))
                },
                assistant: Vec::new(),
                tool_results,
                name: None,
                extra,
            })
        }
        "assistant" => {
            let mut message = Message {
                role: Role::Assistant,
                content: None,
                assistant: Vec::new(),
                tool_results: Vec::new(),
                name: None,
                extra,
            };
            for block in blocks {
                if let Some(content) = decode_assistant_block(block) {
                    message.assistant.push(content);
                }
            }
            Ok(message)
        }
        other => Err(ProtocolError::new(format!(
            "unsupported anthropic message role `{other}`"
        ))),
    }
}

fn extra_of(block: &Value, keys: &[&str]) -> Map<String, Value> {
    let mut extra = Map::new();
    for key in keys {
        if let Some(value) = block.get(key) {
            extra.insert((*key).to_string(), value.clone());
        }
    }
    extra
}

fn decode_tool_result(block: &Value) -> ProtocolResult<octra_llm_core::ToolResult> {
    let tool_call_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("tool_result blocks need `tool_use_id`"))?
        .to_string();
    let content = match block.get("content") {
        Some(Value::String(text)) => UserContent::text(text.clone()),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("image") => {
                        parts.push(ContentPart::Image(decode_attachment(item, "image")))
                    }
                    Some("document") => {
                        parts.push(ContentPart::File(decode_attachment(item, "file")))
                    }
                    _ => parts.push(ContentPart::Text {
                        text: item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        extra: Map::new(),
                    }),
                }
            }
            UserContent::from_parts(parts)
        }
        _ => UserContent::text(""),
    };
    Ok(octra_llm_core::ToolResult {
        tool_call_id,
        name: None,
        content,
        is_error: block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn decode_assistant_block(block: &Value) -> Option<AssistantContent> {
    let kind = block.get("type").and_then(Value::as_str)?;
    match kind {
        "text" => Some(AssistantContent::Text {
            text: block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "thinking" => Some(AssistantContent::Reasoning {
            text: block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "tool_use" => Some(AssistantContent::ToolCall {
            id: block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("toolu_missing")
                .to_string(),
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: block.get("input").cloned().unwrap_or(json!({})),

            kind: None,
        }),
        other => Some(AssistantContent::Raw {
            kind: other.to_string(),
            raw: block.clone(),
        }),
    }
}

fn decode_tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

fn decode_tool(tool: &Value) -> Option<Tool> {
    let name = tool.get("name").and_then(Value::as_str);
    // Custom tools carry `input_schema`; built-in tools carry a versioned
    // `type` such as `web_search_20250305` (or, for MCP toolsets, a name).
    let is_custom = tool.get("input_schema").is_some() || !looks_like_builtin(tool);
    if is_custom && let Some(name) = name {
        return Some(Tool::function(FunctionDefinition {
            name: name.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters: tool.get("input_schema").cloned(),
            strict: None,
        }));
    }
    Some(Tool {
        kind: ToolCallKind::Provider,
        name: name.map(str::to_string),
        function: None,
        provider_type: tool
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| name.map(str::to_string)),
        provider_config: Some(tool.clone()),
        extra: Map::new(),
    })
}

fn looks_like_builtin(tool: &Value) -> bool {
    let Some(kind) = tool.get("type").and_then(Value::as_str) else {
        return true;
    };
    matches!(
        kind,
        "web_search"
            | "web_fetch"
            | "code_execution"
            | "bash"
            | "text_editor"
            | "computer"
            | "memory"
            | "tool_search"
    ) || kind.contains('_')
}

pub fn encode_request(request: &ChatRequest) -> ProtocolResult<Value> {
    let mut object = Map::new();
    object.insert("model".to_string(), json!(request.model));
    object.insert(
        "max_tokens".to_string(),
        json!(request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );

    if !request.system.is_empty() {
        if request.system.blocks.len() == 1 {
            object.insert("system".to_string(), json!(request.system.blocks[0]));
        } else {
            let blocks: Vec<Value> = request
                .system
                .blocks
                .iter()
                .map(|text| json!({"type": "text", "text": text}))
                .collect();
            object.insert("system".to_string(), Value::Array(blocks));
        }
    }

    let mut messages = Vec::new();
    for message in &request.messages {
        encode_message(message, &mut messages)?;
    }
    merge_adjacent_messages(&mut messages);
    object.insert("messages".to_string(), Value::Array(messages));

    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        object.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(top_k) = request.top_k {
        object.insert("top_k".to_string(), json!(top_k));
    }
    if !request.stop.is_empty() {
        object.insert("stop_sequences".to_string(), json!(request.stop));
    }
    if request.stream == Some(true) {
        object.insert("stream".to_string(), json!(true));
    }

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .filter_map(|tool| match tool.kind {
                ToolCallKind::Function => tool.function.as_ref().map(|function| {
                    let mut spec = Map::new();
                    spec.insert("name".to_string(), json!(function.name));
                    if let Some(description) = &function.description {
                        spec.insert("description".to_string(), json!(description));
                    }
                    spec.insert(
                        "input_schema".to_string(),
                        function
                            .parameters
                            .clone()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    );
                    Value::Object(spec)
                }),
                // Provider tools already exist in Anthropic shape; ones that
                // came from OpenAI custom tools are not forwardable.
                ToolCallKind::Provider => tool.provider_config.clone().filter(|config| {
                    config.get("input_schema").is_none()
                        && config.get("type").and_then(Value::as_str).is_some()
                }),
            })
            .collect();
        if !tools.is_empty() {
            object.insert("tools".to_string(), Value::Array(tools));
        }
    }

    if let Some(choice) = &request.tool_choice
        && let Some(choice) = encode_tool_choice(choice, request.parallel_tool_calls)
    {
        object.insert("tool_choice".to_string(), choice);
    } else if request.tool_choice.is_some() && request.parallel_tool_calls == Some(false) {
        object.insert(
            "tool_choice".to_string(),
            json!({"type": "auto", "disable_parallel_tool_use": true}),
        );
    }
    if request.parallel_tool_calls == Some(false) && !object.contains_key("tool_choice") {
        object.insert(
            "tool_choice".to_string(),
            json!({"type": "auto", "disable_parallel_tool_use": true}),
        );
    }

    if let Some(format) = &request.response_format {
        let schema = format
            .get("json_schema")
            .or_else(|| format.get("schema"))
            .cloned()
            .unwrap_or_else(|| format.clone());
        object.insert(
            "output_format".to_string(),
            json!({"type": "json_schema", "json_schema": schema}),
        );
    }

    if let Some(effort) = &request.reasoning_effort
        && request.extra.get("anthropic_thinking").is_none()
    {
        let budget = match effort.as_str() {
            "low" => 1024,
            "high" => 32_000,
            _ => 8_000,
        };
        object.insert(
            "thinking".to_string(),
            json!({"type": "enabled", "budget_tokens": budget}),
        );
    }

    for (key, value) in &request.extra {
        if key == "anthropic_thinking" {
            object
                .entry("thinking".to_string())
                .or_insert_with(|| value.clone());
        } else if !JSON_ONLY_KEYS.contains(&key.as_str()) {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    Ok(Value::Object(object))
}

const JSON_ONLY_KEYS: [&str; 9] = [
    "n",
    "logprobs",
    "top_logprobs",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "seed",
    "stream_options",
    "user",
];

fn encode_tool_choice(choice: &Value, parallel: Option<bool>) -> Option<Value> {
    let mut object = Map::new();
    match choice {
        Value::String(kind) => {
            let encoded = match kind.as_str() {
                "auto" => "auto",
                "required" | "any" => "any",
                "none" => "none",
                _ => return None,
            };
            object.insert("type".to_string(), json!(encoded));
        }
        Value::Object(_) => {
            let name = choice
                .get("function")
                .and_then(|function| function.get("name"))
                .or_else(|| choice.get("name"))?;
            object.insert("type".to_string(), json!("tool"));
            object.insert("name".to_string(), name.clone());
        }
        _ => return None,
    }
    if parallel == Some(false) {
        object.insert("disable_parallel_tool_use".to_string(), json!(true));
    }
    Some(Value::Object(object))
}

fn encode_message(message: &Message, out: &mut Vec<Value>) -> ProtocolResult<()> {
    match message.role {
        Role::System | Role::Developer => {
            // System messages that arrive in the message list are folded into
            // a leading user text block to stay valid for Anthropic servers.
            let text = message
                .content
                .as_ref()
                .map(|content| {
                    content
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            out.push(json!({"role": "user", "content": [{"type": "text", "text": text}]}));
        }
        Role::User => {
            let mut blocks = Vec::new();
            if let Some(content) = &message.content {
                for part in &content.parts {
                    match part {
                        ContentPart::Text { text, .. } => {
                            blocks.push(json!({"type": "text", "text": text}))
                        }
                        ContentPart::Image(image) => blocks.push(encode_image(image)),
                        ContentPart::File(file) => blocks.push(encode_document(file)),
                        ContentPart::Reasoning { .. } | ContentPart::Unsupported { .. } => {}
                    }
                }
            }
            for result in &message.tool_results {
                let content = match result.content.as_text() {
                    Some(text) => json!(text),
                    None => {
                        let mut parts = Vec::new();
                        for part in &result.content.parts {
                            match part {
                                ContentPart::Image(image) => parts.push(encode_image(image)),
                                ContentPart::File(file) => parts.push(encode_document(file)),
                                ContentPart::Text { text, .. } => {
                                    parts.push(json!({"type": "text", "text": text}))
                                }
                                _ => {}
                            }
                        }
                        Value::Array(parts)
                    }
                };
                let mut block = Map::new();
                block.insert("type".to_string(), json!("tool_result"));
                block.insert("tool_use_id".to_string(), json!(result.tool_call_id));
                block.insert("content".to_string(), content);
                if result.is_error {
                    block.insert("is_error".to_string(), json!(true));
                }
                blocks.push(Value::Object(block));
            }
            if blocks.is_empty() {
                blocks.push(json!({"type": "text", "text": ""}));
            }
            out.push(json!({"role": "user", "content": blocks}));
        }
        Role::Assistant => {
            let mut blocks = Vec::new();
            for block in &message.assistant {
                match block {
                    AssistantContent::Text { text } => {
                        blocks.push(json!({"type": "text", "text": text}))
                    }
                    AssistantContent::Reasoning { text, signature } => {
                        let mut thinking = Map::new();
                        thinking.insert("type".to_string(), json!("thinking"));
                        thinking.insert("thinking".to_string(), json!(text));
                        if let Some(signature) = signature {
                            thinking.insert("signature".to_string(), json!(signature));
                        }
                        blocks.push(Value::Object(thinking));
                    }
                    AssistantContent::ToolCall {
                        id,
                        name,
                        arguments,
                        kind: _,
                    } => blocks.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": arguments,
                    })),
                    AssistantContent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                        "is_error": is_error,
                    })),
                    AssistantContent::Refusal { text } => {
                        blocks.push(json!({"type": "text", "text": text}))
                    }
                    AssistantContent::Raw { kind, raw } if kind != "tool_call" => {
                        // Opus 4.7 tool-call blocks are forwarded verbatim so
                        // they keep their full identity.
                        blocks.push(raw.clone());
                    }
                    AssistantContent::Raw { .. } => {}
                }
            }
            if blocks.is_empty() {
                blocks.push(json!({"type": "text", "text": ""}));
            }
            out.push(json!({"role": "assistant", "content": blocks}));
        }
        Role::Tool => {
            // Orphan tool results (no assistant turn) are sent as a user turn.
            let mut pending = message.clone();
            pending.role = Role::User;
            pending.content = None;
            encode_message(&pending, out)?;
        }
    }
    Ok(())
}

fn encode_image(image: &Attachment) -> Value {
    let mut block = Map::new();
    block.insert("type".to_string(), json!("image"));
    match (&image.data, &image.url) {
        (Some(AttachmentData::Base64 { data }), _) => {
            block.insert(
                "source".to_string(),
                json!({"type": "base64", "media_type": image.media_type, "data": data}),
            );
        }
        (_, Some(url)) => {
            block.insert("source".to_string(), json!({"type": "url", "url": url}));
        }
        _ => {
            block.insert(
                "source".to_string(),
                json!({"type": "base64", "media_type": image.media_type, "data": ""}),
            );
        }
    }
    if let Some(cache_control) = image.extra.get("cache_control") {
        block.insert("cache_control".to_string(), cache_control.clone());
    }
    Value::Object(block)
}

fn encode_document(file: &Attachment) -> Value {
    let mut block = Map::new();
    block.insert("type".to_string(), json!("document"));
    match (&file.data, &file.url) {
        (Some(AttachmentData::Base64 { data }), _) => {
            block.insert(
                "source".to_string(),
                json!({"type": "base64", "media_type": file.media_type, "data": data}),
            );
        }
        (_, Some(url)) => {
            block.insert("source".to_string(), json!({"type": "url", "url": url}));
        }
        _ => {
            block.insert(
                "source".to_string(),
                json!({"type": "text", "media_type": file.media_type, "data": ""}),
            );
        }
    }
    if let Some(name) = &file.name {
        block.insert("title".to_string(), json!(name));
    }
    if let Some(cache_control) = file.extra.get("cache_control") {
        block.insert("cache_control".to_string(), cache_control.clone());
    }
    Value::Object(block)
}

/// Anthropic rejects two consecutive turns with the same role.
fn merge_adjacent_messages(messages: &mut Vec<Value>) {
    let mut merged: Vec<Value> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        if let Some(last) = merged.last_mut()
            && last.get("role") == message.get("role")
        {
            let prev = last.get_mut("content").and_then(Value::as_array_mut);
            let next = message.get("content").and_then(Value::as_array).cloned();
            if let (Some(prev), Some(next)) = (prev, next) {
                prev.extend(next);
                continue;
            }
        }
        merged.push(message);
    }
    *messages = merged;
}
