//! Gemini generateContent requests to/from canonical IR.

use octra_llm_core::{
    AssistantContent, Attachment, AttachmentData, ChatRequest, ContentPart, FunctionDefinition,
    Map, Message, ProtocolError, ProtocolResult, Role, SystemPrompt, Tool, ToolCallKind,
    ToolResult, UserContent, Value,
};
use serde_json::json;

pub fn decode_request(value: Value) -> ProtocolResult<ChatRequest> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("request body must be a JSON object"))?;

    let mut system = SystemPrompt::default();
    if let Some(instruction) = object.get("systemInstruction") {
        for part in parts_of(instruction) {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                system.blocks.push(text.to_string());
            }
        }
    }

    let contents = object
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("missing array field `contents`"))?;
    let mut messages = Vec::new();
    let mut call_ids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for content in contents {
        let role = content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let parts = parts_of(content);
        if role == "model" || role == "assistant" {
            let mut message = Message::new(Role::Assistant);
            for part in parts {
                decode_model_part(
                    part,
                    &mut message.assistant,
                    &mut message.extra,
                    &mut call_ids,
                );
            }
            messages.push(message);
        } else {
            let mut user_parts = Vec::new();
            let mut tool_results = Vec::new();
            for part in parts {
                if let Some(response) = part.get("functionResponse") {
                    let name = response
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let text = response
                        .get("response")
                        .map(|value| {
                            value
                                .get("content")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| value.to_string())
                        })
                        .unwrap_or_default();
                    let tool_call_id = call_ids
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| format!("call_{name}"));
                    tool_results.push(ToolResult {
                        tool_call_id,
                        name: Some(name),
                        content: UserContent::text(text),
                        is_error: false,
                    });
                } else {
                    user_parts.push(decode_user_part(part));
                }
            }
            if tool_results.is_empty() {
                messages.push(Message::user_parts(user_parts));
            } else {
                messages.push(Message {
                    role: Role::Tool,
                    content: (!user_parts.is_empty()).then(|| UserContent::from_parts(user_parts)),
                    assistant: Vec::new(),
                    tool_results,
                    name: None,
                    extra: Map::new(),
                });
            }
        }
    }
    if messages.is_empty() {
        return Err(ProtocolError::new(
            "`contents` must contain at least one item",
        ));
    }

    let mut request = ChatRequest::new("gemini", messages);
    request.system = system;
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if let Some(functions) = tool.get("functionDeclarations").and_then(Value::as_array) {
                for function in functions {
                    if let Some(function) = decode_function(function) {
                        request.tools.push(function);
                    }
                }
            } else {
                request.tools.push(Tool {
                    kind: ToolCallKind::Provider,
                    name: tool.get("name").and_then(Value::as_str).map(str::to_string),
                    function: None,
                    provider_type: Some("google_search".to_string()),
                    provider_config: Some(tool.clone()),
                    extra: Map::new(),
                });
            }
        }
    }
    request.tool_choice = decode_tool_config(object.get("toolConfig"));

    if let Some(generation) = object.get("generationConfig") {
        request.temperature = generation.get("temperature").and_then(Value::as_f64);
        request.top_p = generation.get("topP").and_then(Value::as_f64);
        request.top_k = generation
            .get("topK")
            .and_then(Value::as_u64)
            .map(|value| value.min(u32::MAX as u64) as u32);
        request.max_tokens = generation
            .get("maxOutputTokens")
            .and_then(Value::as_u64)
            .map(|value| value.min(u32::MAX as u64) as u32);
        request.stop = generation
            .get("stopSequences")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(schema) = generation.get("responseSchema") {
            request.response_format = Some(json!({
                "type": "json_schema",
                "json_schema": {"schema": schema},
            }));
        } else if generation.get("responseMimeType").and_then(Value::as_str)
            == Some("application/json")
        {
            request.response_format = Some(json!({"type": "json_object"}));
        }
        request.reasoning_effort = generation
            .get("thinkingConfig")
            .and_then(|config| config.get("thinkingBudget"))
            .map(|budget| match budget.as_u64() {
                Some(0) => "none".to_string(),
                Some(value) if value <= 1024 => "low".to_string(),
                Some(value) if value <= 8192 => "medium".to_string(),
                _ => "high".to_string(),
            });
    }

    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "contents" | "systemInstruction" | "tools" | "toolConfig" | "generationConfig"
        ) {
            request.extra.insert(key.clone(), value.clone());
        }
    }
    Ok(request)
}

fn parts_of(value: &Value) -> &[Value] {
    value
        .get("parts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn decode_function(value: &Value) -> Option<Tool> {
    Some(Tool::function(FunctionDefinition {
        name: value.get("name")?.as_str()?.to_string(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        parameters: value.get("parameters").cloned(),
        strict: None,
    }))
}

fn decode_tool_config(value: Option<&Value>) -> Option<Value> {
    let config = value?.get("functionCallingConfig")?;
    match config.get("mode").and_then(Value::as_str) {
        Some("ANY") => {
            let names = config
                .get("allowedFunctionNames")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            match names.first().and_then(Value::as_str) {
                Some(name) => Some(json!({"type": "function", "name": name})),
                None => Some(json!("required")),
            }
        }
        Some("NONE") => Some(json!("none")),
        _ => None,
    }
}

fn decode_user_part(part: &Value) -> ContentPart {
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        return ContentPart::Text {
            text: text.to_string(),
            extra: Map::new(),
        };
    }
    if let Some(inline) = part.get("inlineData") {
        let media_type = inline
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        let data = inline
            .get("data")
            .and_then(Value::as_str)
            .map(|data| AttachmentData::Base64 {
                data: data.to_string(),
            });
        let attachment = Attachment {
            media_type: media_type.clone(),
            data,
            url: None,
            name: None,
            extra: Map::new(),
        };
        return if media_type.starts_with("image/") {
            ContentPart::Image(attachment)
        } else {
            ContentPart::File(attachment)
        };
    }
    if let Some(file) = part.get("fileData") {
        let media_type = file
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        let attachment = Attachment {
            media_type: media_type.clone(),
            data: None,
            url: file
                .get("fileUri")
                .and_then(Value::as_str)
                .map(str::to_string),
            name: None,
            extra: Map::new(),
        };
        return if media_type.starts_with("image/") {
            ContentPart::Image(attachment)
        } else {
            ContentPart::File(attachment)
        };
    }
    ContentPart::Unsupported {
        kind: "gemini_part".to_string(),
        raw: part.clone(),
    }
}

fn decode_model_part(
    part: &Value,
    assistant: &mut Vec<AssistantContent>,
    extra: &mut Map<String, Value>,
    call_ids: &mut std::collections::HashMap<String, String>,
) {
    if let Some(call) = part.get("functionCall") {
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = format!("call_{name}_{}", call_ids.len());
        call_ids.insert(name.clone(), id.clone());
        if let Some(signature) = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|signature| !signature.is_empty())
        {
            let signatures = extra
                .entry("gemini_thought_signatures")
                .or_insert_with(|| json!({}));
            if let Some(signatures) = signatures.as_object_mut() {
                signatures.insert(id.clone(), Value::String(signature.to_string()));
            }
        }
        assistant.push(AssistantContent::ToolCall {
            id,
            name,
            arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
            kind: Some(ToolCallKind::Function),
        });
        return;
    }
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        if part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            assistant.push(AssistantContent::Reasoning {
                text: text.to_string(),
                signature: part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        } else {
            assistant.push(AssistantContent::Text {
                text: text.to_string(),
            });
        }
    }
}

pub fn encode_request(request: &ChatRequest) -> ProtocolResult<Value> {
    let mut object = Map::new();
    if !request.system.is_empty() {
        object.insert(
            "systemInstruction".to_string(),
            json!({"parts": request.system.blocks.iter().map(|text| json!({"text": text})).collect::<Vec<_>>()}),
        );
    }

    let mut contents = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {}
            Role::User => {
                let parts: Vec<Value> = message
                    .content
                    .as_ref()
                    .map(|content| encode_user_parts(&content.parts))
                    .unwrap_or_default();
                contents.push(json!({"role": "user", "parts": parts}));
            }
            Role::Assistant => {
                let mut parts = Vec::new();
                for block in &message.assistant {
                    match block {
                        AssistantContent::Text { text } => parts.push(json!({"text": text})),
                        AssistantContent::Reasoning { text, signature } => {
                            let mut part = json!({"text": text, "thought": true});
                            if let Some(signature) = signature {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part)
                        }
                        AssistantContent::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            let mut part = json!({
                                "functionCall": {"name": name, "args": arguments},
                            });
                            if let Some(signature) = message
                                .extra
                                .get("gemini_thought_signatures")
                                .and_then(Value::as_object)
                                .and_then(|signatures| signatures.get(id))
                                .and_then(Value::as_str)
                            {
                                part["thoughtSignature"] = json!(signature);
                            }
                            parts.push(part)
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
            }
            Role::Tool => {
                let mut parts = Vec::new();
                for result in &message.tool_results {
                    let text = result
                        .content
                        .parts
                        .iter()
                        .filter_map(ContentPart::as_text)
                        .collect::<Vec<_>>()
                        .join("");
                    let name = result.name.clone().unwrap_or_else(|| "tool".to_string());
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"content": text},
                        }
                    }));
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
        }
    }
    object.insert("contents".to_string(), Value::Array(contents));

    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .filter_map(|tool| match tool.kind {
                ToolCallKind::Function => tool.function.as_ref().map(|function| {
                    let mut spec = Map::new();
                    spec.insert("name".to_string(), json!(function.name));
                    if let Some(description) = &function.description {
                        spec.insert("description".to_string(), json!(description));
                    }
                    if let Some(parameters) = &function.parameters {
                        spec.insert("parameters".to_string(), parameters.clone());
                    }
                    Value::Object(spec)
                }),
                ToolCallKind::Provider => None,
            })
            .collect();
        if !declarations.is_empty() {
            object.insert(
                "tools".to_string(),
                json!([{"functionDeclarations": declarations}]),
            );
        }
    }

    if let Some(choice) = &request.tool_choice {
        let config = match choice {
            Value::String(value) if value == "none" => json!({"mode": "NONE"}),
            Value::String(value) if value == "required" || value == "any" => {
                json!({"mode": "ANY"})
            }
            Value::Object(_) => {
                let name = choice
                    .get("name")
                    .or_else(|| choice.get("function").and_then(|f| f.get("name")))
                    .and_then(Value::as_str);
                match name {
                    Some(name) => {
                        json!({"mode": "ANY", "allowedFunctionNames": [name]})
                    }
                    None => json!({"mode": "AUTO"}),
                }
            }
            _ => json!({"mode": "AUTO"}),
        };
        object.insert(
            "toolConfig".to_string(),
            json!({"functionCallingConfig": config}),
        );
    }

    let mut generation = Map::new();
    if let Some(temperature) = request.temperature {
        generation.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        generation.insert("topP".to_string(), json!(top_p));
    }
    if let Some(top_k) = request.top_k {
        generation.insert("topK".to_string(), json!(top_k));
    }
    if let Some(max_tokens) = request.max_tokens {
        generation.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if !request.stop.is_empty() {
        generation.insert("stopSequences".to_string(), json!(request.stop));
    }
    if let Some(format) = &request.response_format {
        if format.get("type").and_then(Value::as_str) == Some("json_object") {
            generation.insert("responseMimeType".to_string(), json!("application/json"));
        } else if let Some(schema) = format
            .get("json_schema")
            .and_then(|schema| schema.get("schema"))
        {
            generation.insert("responseMimeType".to_string(), json!("application/json"));
            generation.insert("responseSchema".to_string(), schema.clone());
        }
    }
    if let Some(effort) = &request.reasoning_effort {
        let budget = match effort.as_str() {
            "none" | "minimal" => 0,
            "low" => 1024,
            "medium" => 8192,
            _ => 24576,
        };
        generation.insert(
            "thinkingConfig".to_string(),
            json!({"thinkingBudget": budget}),
        );
    }
    if !generation.is_empty() {
        object.insert("generationConfig".to_string(), Value::Object(generation));
    }

    for (key, value) in &request.extra {
        object.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Ok(Value::Object(object))
}

fn encode_user_parts(parts: &[ContentPart]) -> Vec<Value> {
    let mut encoded = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text, .. } => encoded.push(json!({"text": text})),
            ContentPart::Image(image) | ContentPart::File(image) => {
                if let Some(AttachmentData::Base64 { data }) = &image.data {
                    encoded.push(json!({
                        "inlineData": {"mimeType": image.media_type, "data": data},
                    }));
                } else if let Some(url) = &image.url {
                    encoded.push(json!({
                        "fileData": {"mimeType": image.media_type, "fileUri": url},
                    }));
                }
            }
            ContentPart::Reasoning { text, .. } => encoded.push(json!({"text": text})),
            ContentPart::Unsupported { raw, .. } => encoded.push(raw.clone()),
        }
    }
    encoded
}
