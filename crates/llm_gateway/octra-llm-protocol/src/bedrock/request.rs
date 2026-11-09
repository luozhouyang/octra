//! Bedrock Converse request mapping.
//!
//! The canonical IR already stores attachment payloads as raw base64 without a
//! data-URL prefix, so image and document bytes are forwarded as-is. A stray
//! `data:...;base64,` prefix is stripped defensively.
//!
//! Media that has no inline bytes is dropped: Converse only accepts inline
//! bytes or S3 locations, and the IR never records which S3 bucket a URL came
//! from. If dropping leaves a message empty, an empty text block is emitted so
//! the turn stays valid. Tool-call arguments that are not valid JSON become an
//! empty object because Converse requires a JSON document.
//!
//! Unknown top-level request options (`response_format`, `reasoning_effort`,
//! `user`, and the IR `extra` map) are intentionally not forwarded because
//! Converse has no equivalent documented field and guessing at additional
//! model request fields would be model-specific.

use octra_llm_core::{
    AssistantContent, AttachmentData, ChatRequest, ContentPart, FunctionDefinition, Map, Message,
    ProtocolError, ProtocolResult, Role, SystemPrompt, Tool, ToolCallKind, ToolResult, UserContent,
    Value,
};
use serde_json::json;

pub fn encode_request(request: &ChatRequest) -> ProtocolResult<Value> {
    let mut object = Map::new();
    let mut system: Vec<Value> = request
        .system
        .blocks
        .iter()
        .map(|text| json!({"text": text}))
        .collect();

    let mut messages: Vec<Value> = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {
                // Converse has no system role inside `messages`; fold those
                // turns into the leading system prompt.
                if let Some(content) = &message.content {
                    for part in &content.parts {
                        if let ContentPart::Text { text, .. } = part {
                            system.push(json!({"text": text}));
                        }
                    }
                }
            }
            Role::User => {
                let mut blocks = Vec::new();
                if let Some(content) = &message.content {
                    encode_user_parts(content, &mut blocks);
                }
                for result in &message.tool_results {
                    blocks.push(encode_tool_result(result));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"text": ""}));
                }
                push_message(&mut messages, "user", blocks);
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                for block in &message.assistant {
                    match block {
                        AssistantContent::Text { text } => blocks.push(json!({"text": text})),
                        AssistantContent::Reasoning { text, signature } => {
                            let mut reasoning_text = Map::new();
                            reasoning_text.insert("text".to_string(), json!(text));
                            if let Some(signature) = signature {
                                reasoning_text.insert("signature".to_string(), json!(signature));
                            }
                            blocks.push(json!({
                                "reasoningContent": {
                                    "reasoningText": Value::Object(reasoning_text),
                                }
                            }));
                        }
                        AssistantContent::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => blocks.push(json!({
                            "toolUse": {
                                "toolUseId": id,
                                "name": name,
                                "input": tool_input(arguments),
                            }
                        })),
                        AssistantContent::Refusal { text } => blocks.push(json!({"text": text})),
                        // Tool results belong to a following user turn; opaque
                        // and provider tool blocks have no Converse shape here.
                        AssistantContent::ToolResult { .. } | AssistantContent::Raw { .. } => {}
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({"text": ""}));
                }
                push_message(&mut messages, "assistant", blocks);
            }
            Role::Tool => {
                let mut blocks = Vec::new();
                for result in &message.tool_results {
                    blocks.push(encode_tool_result(result));
                }
                // An orphan tool-role message may still carry text content.
                if let Some(content) = &message.content {
                    encode_user_parts(content, &mut blocks);
                }
                if blocks.is_empty() {
                    blocks.push(json!({"text": ""}));
                }
                push_message(&mut messages, "user", blocks);
            }
        }
    }

    if !system.is_empty() {
        object.insert("system".to_string(), Value::Array(system));
    }
    object.insert("messages".to_string(), Value::Array(messages));

    let mut inference = Map::new();
    if let Some(max_tokens) = request.max_tokens {
        inference.insert("maxTokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = request.temperature {
        inference.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        inference.insert("topP".to_string(), json!(top_p));
    }
    if !request.stop.is_empty() {
        inference.insert("stopSequences".to_string(), json!(request.stop));
    }
    if !inference.is_empty() {
        object.insert("inferenceConfig".to_string(), Value::Object(inference));
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .filter_map(|tool| match tool.kind {
            ToolCallKind::Function => tool.function.as_ref().map(encode_tool),
            ToolCallKind::Provider => None,
        })
        .collect();
    let mut tool_config = Map::new();
    if !tools.is_empty() {
        tool_config.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(choice) = &request.tool_choice
        && let Some(choice) = encode_tool_choice(choice)
    {
        tool_config.insert("toolChoice".to_string(), choice);
    }
    if !tool_config.is_empty() {
        object.insert("toolConfig".to_string(), Value::Object(tool_config));
    }

    if let Some(top_k) = request.top_k {
        object.insert(
            "additionalModelRequestFields".to_string(),
            json!({"top_k": top_k}),
        );
    }

    Ok(Value::Object(object))
}

/// Best-effort decode for completeness. The ingress router never uses this:
/// Bedrock requests always arrive through a different ingress protocol.
pub fn decode_request(value: Value) -> ProtocolResult<ChatRequest> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("request body must be a JSON object"))?;

    let mut system = SystemPrompt::default();
    if let Some(blocks) = object.get("system").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                system.blocks.push(text.to_string());
            }
        }
    }

    let mut messages = Vec::new();
    if let Some(items) = object.get("messages").and_then(Value::as_array) {
        for item in items {
            messages.push(decode_message(item)?);
        }
    }

    let mut request = ChatRequest::new("bedrock", messages);
    request.system = system;

    if let Some(inference) = object.get("inferenceConfig") {
        request.max_tokens = inference
            .get("maxTokens")
            .and_then(Value::as_u64)
            .map(|value| value.min(u32::MAX as u64) as u32);
        request.temperature = inference.get("temperature").and_then(Value::as_f64);
        request.top_p = inference.get("topP").and_then(Value::as_f64);
        request.stop = inference
            .get("stopSequences")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
    }

    if let Some(tool_config) = object.get("toolConfig") {
        if let Some(tools) = tool_config.get("tools").and_then(Value::as_array) {
            request.tools = tools.iter().filter_map(decode_tool).collect();
        }
        request.tool_choice = decode_tool_choice(tool_config.get("toolChoice"));
    }

    request.top_k = object
        .get("additionalModelRequestFields")
        .and_then(|fields| fields.get("top_k"))
        .and_then(Value::as_u64)
        .map(|value| value.min(u32::MAX as u64) as u32);

    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "system"
                | "messages"
                | "inferenceConfig"
                | "toolConfig"
                | "additionalModelRequestFields"
        ) {
            request.extra.insert(key.clone(), value.clone());
        }
    }

    Ok(request)
}

fn push_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn encode_user_parts(content: &UserContent, blocks: &mut Vec<Value>) {
    for part in &content.parts {
        match part {
            ContentPart::Text { text, .. } => blocks.push(json!({"text": text})),
            ContentPart::Image(image) => {
                let Some(data) = inline_bytes(image.data.as_ref()) else {
                    continue;
                };
                let Some(format) = image_format(&image.media_type) else {
                    continue;
                };
                blocks.push(json!({
                    "image": {
                        "format": format,
                        "source": {"bytes": data},
                    }
                }));
            }
            ContentPart::File(file) => {
                let Some(data) = inline_bytes(file.data.as_ref()) else {
                    continue;
                };
                let format = document_format(&file.media_type, file.name.as_deref());
                let name = file.name.clone().unwrap_or_else(|| "document".to_string());
                blocks.push(json!({
                    "document": {
                        "format": format,
                        "name": name,
                        "source": {"bytes": data},
                    }
                }));
            }
            ContentPart::Reasoning { text, .. } => blocks.push(json!({"text": text})),
            ContentPart::Unsupported { .. } => {}
        }
    }
}

fn encode_tool_result(result: &ToolResult) -> Value {
    let mut content = Vec::new();
    for part in &result.content.parts {
        match part {
            ContentPart::Text { text, .. } => {
                // Tool results are commonly JSON documents; send them as
                // structured JSON when possible and fall back to plain text.
                match serde_json::from_str::<Value>(text.trim()) {
                    Ok(value) => content.push(json!({"json": value})),
                    Err(_) => content.push(json!({"text": text})),
                }
            }
            ContentPart::Image(image) => {
                if let Some(data) = inline_bytes(image.data.as_ref())
                    && let Some(format) = image_format(&image.media_type)
                {
                    content.push(json!({
                        "image": {
                            "format": format,
                            "source": {"bytes": data},
                        }
                    }));
                }
            }
            ContentPart::File(file) => {
                if let Some(data) = inline_bytes(file.data.as_ref()) {
                    content.push(json!({
                        "document": {
                            "format": document_format(&file.media_type, file.name.as_deref()),
                            "name": file.name.clone().unwrap_or_else(|| "document".to_string()),
                            "source": {"bytes": data},
                        }
                    }));
                }
            }
            ContentPart::Reasoning { text, .. } => content.push(json!({"text": text})),
            ContentPart::Unsupported { .. } => {}
        }
    }
    if content.is_empty() {
        content.push(json!({"text": ""}));
    }
    json!({
        "toolResult": {
            "toolUseId": result.tool_call_id,
            "content": content,
            "status": if result.is_error { "error" } else { "success" },
        }
    })
}

fn encode_tool(function: &FunctionDefinition) -> Value {
    let mut spec = Map::new();
    spec.insert("name".to_string(), json!(function.name));
    if let Some(description) = &function.description {
        spec.insert("description".to_string(), json!(description));
    }
    spec.insert(
        "inputSchema".to_string(),
        json!({
            "json": function
                .parameters
                .clone()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        }),
    );
    json!({"toolSpec": Value::Object(spec)})
}

fn encode_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(kind) => match kind.as_str() {
            "auto" => Some(json!({"auto": {}})),
            "required" | "any" => Some(json!({"any": {}})),
            _ => None,
        },
        Value::Object(_) => {
            let name = choice
                .get("function")
                .and_then(|function| function.get("name"))
                .or_else(|| choice.get("name"))?;
            Some(json!({"tool": {"name": name}}))
        }
        _ => None,
    }
}

fn tool_input(arguments: &Value) -> Value {
    match arguments {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    }
}

/// Returns raw base64 without a data-URL prefix.
fn inline_bytes(data: Option<&AttachmentData>) -> Option<String> {
    match data? {
        AttachmentData::Base64 { data } => Some(strip_data_url(data)),
    }
}

fn strip_data_url(data: &str) -> String {
    let trimmed = data.trim();
    let payload = match trimmed.strip_prefix("data:") {
        Some(rest) => rest
            .split_once(',')
            .map(|(meta, payload)| {
                if meta.to_ascii_lowercase().contains(";base64") {
                    payload
                } else {
                    ""
                }
            })
            .unwrap_or(""),
        None => trimmed,
    };
    payload
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect()
}

fn image_format(media_type: &str) -> Option<&'static str> {
    let media_type = media_type.trim().to_ascii_lowercase();
    match media_type.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpeg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn document_format(media_type: &str, name: Option<&str>) -> &'static str {
    let media_type = media_type.trim().to_ascii_lowercase();
    match media_type.as_str() {
        "application/pdf" => return "pdf",
        "text/csv" => return "csv",
        "text/html" => return "html",
        "text/markdown" | "text/x-markdown" => return "md",
        "text/plain" => return "txt",
        "application/msword" => return "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => return "docx",
        "application/vnd.ms-excel" => return "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => return "xlsx",
        _ => {}
    }
    let extension = name
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("pdf") => "pdf",
        Some("csv") => "csv",
        Some("doc") => "doc",
        Some("docx") => "docx",
        Some("xls") => "xls",
        Some("xlsx") => "xlsx",
        Some("html") | Some("htm") => "html",
        Some("md") | Some("markdown") => "md",
        _ => "txt",
    }
}

fn decode_message(value: &Value) -> ProtocolResult<Message> {
    let role = value.get("role").and_then(Value::as_str).unwrap_or("user");
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("each Converse message needs a content array"))?;

    if role == "assistant" {
        let mut message = Message::new(Role::Assistant);
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                message
                    .assistant
                    .push(AssistantContent::Text { text: text.into() });
            } else if let Some(tool_use) = block.get("toolUse") {
                message.assistant.push(AssistantContent::ToolCall {
                    id: tool_use
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .unwrap_or("tooluse_missing")
                        .to_string(),
                    name: tool_use
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: tool_use.get("input").cloned().unwrap_or_else(|| json!({})),
                    kind: None,
                });
            } else if let Some(reasoning) = block
                .get("reasoningContent")
                .and_then(|content| content.get("reasoningText"))
            {
                message.assistant.push(AssistantContent::Reasoning {
                    text: reasoning
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    signature: reasoning
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            } else {
                message.assistant.push(AssistantContent::Raw {
                    kind: "bedrock_content_block".to_string(),
                    raw: block.clone(),
                });
            }
        }
        return Ok(message);
    }

    let mut parts = Vec::new();
    let mut tool_results = Vec::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            parts.push(ContentPart::Text {
                text: text.to_string(),
                extra: Map::new(),
            });
        } else if let Some(image) = block.get("image") {
            parts.push(ContentPart::Image(decode_image(image)));
        } else if let Some(document) = block.get("document") {
            parts.push(ContentPart::File(decode_document(document)));
        } else if let Some(result) = block.get("toolResult") {
            tool_results.push(decode_tool_result(result));
        } else {
            parts.push(ContentPart::Unsupported {
                kind: "bedrock_content_block".to_string(),
                raw: block.clone(),
            });
        }
    }

    Ok(Message {
        role: if role == "user" {
            Role::User
        } else {
            Role::Tool
        },
        content: (!parts.is_empty()).then(|| UserContent::from_parts(parts)),
        assistant: Vec::new(),
        tool_results,
        name: None,
        extra: Map::new(),
    })
}

fn decode_image(value: &Value) -> octra_llm_core::Attachment {
    let format = value.get("format").and_then(Value::as_str).unwrap_or("png");
    octra_llm_core::Attachment {
        media_type: format!("image/{format}"),
        data: value
            .get("source")
            .and_then(|source| source.get("bytes"))
            .and_then(Value::as_str)
            .map(|data| AttachmentData::Base64 {
                data: data.to_string(),
            }),
        url: value
            .get("source")
            .and_then(|source| source.get("s3Location"))
            .and_then(|location| location.get("uri"))
            .and_then(Value::as_str)
            .map(str::to_string),
        name: None,
        extra: Map::new(),
    }
}

fn decode_document(value: &Value) -> octra_llm_core::Attachment {
    let format = value.get("format").and_then(Value::as_str).unwrap_or("txt");
    let media_type = match format {
        "pdf" => "application/pdf",
        "csv" => "text/csv",
        "html" => "text/html",
        "md" => "text/markdown",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "text/plain",
    };
    octra_llm_core::Attachment {
        media_type: media_type.to_string(),
        data: value
            .get("source")
            .and_then(|source| source.get("bytes"))
            .and_then(Value::as_str)
            .map(|data| AttachmentData::Base64 {
                data: data.to_string(),
            }),
        url: value
            .get("source")
            .and_then(|source| source.get("s3Location"))
            .and_then(|location| location.get("uri"))
            .and_then(Value::as_str)
            .map(str::to_string),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        extra: Map::new(),
    }
}

fn decode_tool_result(value: &Value) -> ToolResult {
    let mut parts = Vec::new();
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(ContentPart::text(text));
            } else if let Some(json) = item.get("json") {
                let text = match json {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                parts.push(ContentPart::text(text));
            } else if let Some(image) = item.get("image") {
                parts.push(ContentPart::Image(decode_image(image)));
            } else if let Some(document) = item.get("document") {
                parts.push(ContentPart::File(decode_document(document)));
            }
        }
    }
    ToolResult {
        tool_call_id: value
            .get("toolUseId")
            .and_then(Value::as_str)
            .unwrap_or("tooluse_missing")
            .to_string(),
        name: None,
        content: UserContent::from_parts(parts),
        is_error: value.get("status").and_then(Value::as_str) == Some("error"),
    }
}

fn decode_tool(value: &Value) -> Option<Tool> {
    let spec = value.get("toolSpec")?;
    Some(Tool::function(FunctionDefinition {
        name: spec.get("name")?.as_str()?.to_string(),
        description: spec
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        parameters: spec
            .get("inputSchema")
            .and_then(|schema| schema.get("json"))
            .cloned(),
        strict: None,
    }))
}

fn decode_tool_choice(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if value.get("auto").is_some() {
        Some(json!("auto"))
    } else if value.get("any").is_some() {
        Some(json!("required"))
    } else {
        value
            .get("tool")
            .and_then(|tool| tool.get("name"))
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{
        AssistantContent, Attachment, ContentPart, FunctionDefinition, Message, SystemPrompt, Tool,
        ToolResult, UserContent,
    };
    use serde_json::json;

    #[test]
    fn encodes_system_and_inference_config() {
        let mut request = ChatRequest::new("ignored", vec![Message::user("hi")]);
        request.system = SystemPrompt {
            blocks: vec!["be helpful".to_string(), "be concise".to_string()],
        };
        request.max_tokens = Some(256);
        request.temperature = Some(0.2);
        request.top_p = Some(0.9);
        request.top_k = Some(40);
        request.stop = vec!["END".to_string()];

        let body = encode_request(&request).unwrap();
        assert_eq!(
            body["system"],
            json!([{"text": "be helpful"}, {"text": "be concise"}])
        );
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 256);
        assert_eq!(body["inferenceConfig"]["temperature"], 0.2);
        assert_eq!(body["inferenceConfig"]["topP"], 0.9);
        assert_eq!(body["inferenceConfig"]["stopSequences"], json!(["END"]));
        assert_eq!(body["additionalModelRequestFields"]["top_k"], 40);
        assert!(body.get("model").is_none());
    }

    #[test]
    fn encodes_images_without_data_url_prefixes() {
        let request = ChatRequest::new(
            "m",
            vec![Message::user_parts(vec![
                ContentPart::text("look"),
                ContentPart::Image(Attachment {
                    media_type: "image/png".to_string(),
                    data: Some(AttachmentData::Base64 {
                        data: "data:image/png;base64,aGVsbG8=".to_string(),
                    }),
                    url: None,
                    name: None,
                    extra: Map::new(),
                }),
            ])],
        );
        let body = encode_request(&request).unwrap();
        assert_eq!(
            body["messages"][0]["content"][1],
            json!({
                "image": {
                    "format": "png",
                    "source": {"bytes": "aGVsbG8="},
                }
            })
        );
    }

    #[test]
    fn encodes_assistant_tool_calls_and_tool_results() {
        let mut request = ChatRequest::new("m", vec![Message::user("use a tool")]);
        request.messages.push(Message {
            role: Role::Assistant,
            content: None,
            assistant: vec![AssistantContent::ToolCall {
                id: "tooluse_1".to_string(),
                name: "lookup".to_string(),
                arguments: json!({"q": "x"}),
                kind: None,
            }],
            tool_results: Vec::new(),
            name: None,
            extra: Map::new(),
        });
        request.messages.push(Message {
            role: Role::Tool,
            content: None,
            assistant: Vec::new(),
            tool_results: vec![ToolResult {
                tool_call_id: "tooluse_1".to_string(),
                name: Some("lookup".to_string()),
                content: UserContent::text(r#"{"ok":true}"#),
                is_error: false,
            }],
            name: None,
            extra: Map::new(),
        });

        let body = encode_request(&request).unwrap();
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({
                "toolUse": {
                    "toolUseId": "tooluse_1",
                    "name": "lookup",
                    "input": {"q": "x"},
                }
            })
        );
        assert_eq!(
            body["messages"][2]["content"][0],
            json!({
                "toolResult": {
                    "toolUseId": "tooluse_1",
                    "content": [{"json": {"ok": true}}],
                    "status": "success",
                }
            })
        );
    }

    #[test]
    fn encodes_tool_config_and_choice() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.tools.push(Tool::function(FunctionDefinition {
            name: "lookup".to_string(),
            description: Some("find a thing".to_string()),
            parameters: Some(json!({"type": "object", "properties": {"q": {"type": "string"}}})),
            strict: None,
        }));
        request.tool_choice = Some(json!("required"));

        let body = encode_request(&request).unwrap();
        assert_eq!(
            body["toolConfig"]["tools"][0],
            json!({
                "toolSpec": {
                    "name": "lookup",
                    "description": "find a thing",
                    "inputSchema": {
                        "json": {
                            "type": "object",
                            "properties": {"q": {"type": "string"}},
                        }
                    },
                }
            })
        );
        assert_eq!(body["toolConfig"]["toolChoice"], json!({"any": {}}));

        request.tool_choice = Some(json!({"type": "function", "function": {"name": "lookup"}}));
        let body = encode_request(&request).unwrap();
        assert_eq!(
            body["toolConfig"]["toolChoice"],
            json!({"tool": {"name": "lookup"}})
        );
    }

    #[test]
    fn decodes_a_converse_request() {
        let body = json!({
            "system": [{"text": "be helpful"}],
            "messages": [{
                "role": "user",
                "content": [
                    {"text": "hi"},
                    {
                        "toolResult": {
                            "toolUseId": "tooluse_1",
                            "content": [{"json": {"ok": true}}],
                            "status": "error",
                        }
                    }
                ],
            }],
            "inferenceConfig": {"maxTokens": 64, "temperature": 0.5},
            "toolConfig": {
                "tools": [{
                    "toolSpec": {
                        "name": "lookup",
                        "inputSchema": {"json": {"type": "object"}},
                    }
                }],
                "toolChoice": {"auto": {}},
            },
            "additionalModelRequestFields": {"top_k": 5},
        });
        let request = decode_request(body).unwrap();
        assert_eq!(request.system.blocks, vec!["be helpful".to_string()]);
        assert_eq!(
            request.messages[0].content.as_ref().unwrap().as_text(),
            Some("hi")
        );
        assert_eq!(
            request.messages[0].tool_results[0].tool_call_id,
            "tooluse_1"
        );
        assert!(request.messages[0].tool_results[0].is_error);
        assert_eq!(
            request.messages[0].tool_results[0].content.as_text(),
            Some("{\"ok\":true}")
        );
        assert_eq!(request.max_tokens, Some(64));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.top_k, Some(5));
        assert_eq!(request.tools[0].function_name(), Some("lookup"));
        assert_eq!(request.tool_choice, Some(json!("auto")));
    }
}
