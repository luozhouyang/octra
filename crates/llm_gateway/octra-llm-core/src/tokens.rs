//! Rough request size estimation used for routing hints and token counting.

use crate::types::{AssistantContent, ChatRequest, ContentPart, UserContent};

/// Rough token estimate: about four characters per token for other text and
/// one token per CJK character, plus a small per-part overhead. Only a size
/// hint for routing and `count_tokens`, never used for billing.
pub fn estimate_tokens(request: &ChatRequest) -> u64 {
    let mut total = 0usize;
    for block in &request.system.blocks {
        total += text_tokens(block);
    }
    for message in &request.messages {
        if let Some(content) = &message.content {
            total += content_tokens(content);
        }
        for block in &message.assistant {
            match block {
                AssistantContent::Text { text } => total += text_tokens(text),
                AssistantContent::Reasoning { text, .. } => total += text_tokens(text),
                AssistantContent::ToolCall { arguments, .. } => {
                    total += text_tokens(&arguments.to_string())
                }
                _ => {}
            }
        }
        for result in &message.tool_results {
            total += content_tokens(&result.content);
        }
    }
    for tool in &request.tools {
        if let Some(function) = &tool.function {
            total += text_tokens(&function.name);
            if let Some(description) = &function.description {
                total += text_tokens(description);
            }
            if let Some(parameters) = &function.parameters {
                total += text_tokens(&parameters.to_string());
            }
        }
    }
    total as u64
}

fn text_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if matches!(ch, '\u{3040}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ac00}'..='\u{d7af}')
        {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4) + 4
}

fn content_tokens(content: &UserContent) -> usize {
    content
        .parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text, .. } => text_tokens(text),
            ContentPart::Reasoning { text, .. } => text_tokens(text),
            ContentPart::Image(_) | ContentPart::File(_) => 1000,
            ContentPart::Unsupported { .. } => 4,
        })
        .sum()
}
