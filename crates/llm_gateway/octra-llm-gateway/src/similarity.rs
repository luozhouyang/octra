//! Local lexical similarity used by semantic routing.
//!
//! Routing must not depend on an external embedding service, so this module
//! provides a deterministic feature-hashed vector: lowercase alphanumeric
//! words plus character trigrams mapped into 256 dimensions with FNV-1a and
//! L2 normalized. The algorithm mirrors the semantic cache plugin's local
//! vector while remaining internal to the gateway crate.

use octra_llm_core::{AssistantContent, ChatRequest, ContentPart};

/// Local lexical vectors always use this many feature-hashed dimensions.
const VECTOR_DIMS: usize = 256;

/// Stable FNV-1a 64-bit hash. Similarity vectors must not depend on
/// `DefaultHasher`, whose output is not stable across processes.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn add_feature(vector: &mut [f32], feature: &str) {
    let index = (fnv1a64(feature.as_bytes()) as usize) % vector.len();
    vector[index] += 1.0;
}

/// Builds the deterministic lexical vector for `text`.
///
/// Lowercase alphanumeric words plus character trigrams are feature-hashed
/// into [`VECTOR_DIMS`] dimensions and L2 normalized. Empty or
/// whitespace-only input yields an all-zero vector.
pub fn lexical_vector(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let mut vector = vec![0.0f32; VECTOR_DIMS];
    for word in lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        add_feature(&mut vector, word);
    }
    let characters: Vec<char> = lower.chars().collect();
    for window in characters.windows(3) {
        if window.iter().all(|character| character.is_whitespace()) {
            continue;
        }
        let trigram: String = window.iter().collect();
        add_feature(&mut vector, &trigram);
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

/// Cosine similarity between two equal-length vectors. Mismatched or empty
/// vectors score `0.0`.
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    if left.is_empty() || left.len() != right.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (left, right) in left.iter().zip(right) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// Concatenated system text plus text blocks in messages. Tool-call arguments
/// are structured values and intentionally excluded, as are images and files.
pub fn request_text(request: &ChatRequest, max_chars: usize) -> String {
    let mut out = String::new();
    let mut push = |text: &str| {
        if out.is_empty() {
            out.push_str(text);
        } else {
            out.push('\n');
            out.push_str(text);
        }
    };
    for block in &request.system.blocks {
        push(block);
    }
    for message in &request.messages {
        if let Some(content) = &message.content {
            for part in &content.parts {
                if let ContentPart::Text { text, .. } = part {
                    push(text);
                }
            }
        }
        for block in &message.assistant {
            if let AssistantContent::Text { text } = block {
                push(text);
            }
        }
        for result in &message.tool_results {
            for part in &result.content.parts {
                if let ContentPart::Text { text, .. } = part {
                    push(text);
                }
            }
        }
    }
    truncate_chars(&out, max_chars)
}

/// Truncates to at most `limit` Unicode scalar values without splitting one.
pub(crate) fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{Attachment, Message, Role, SystemPrompt, ToolResult, UserContent};
    use serde_json::json;

    fn norm(vector: &[f32]) -> f32 {
        vector.iter().map(|value| value * value).sum::<f32>().sqrt()
    }

    fn attachment(media_type: &str) -> Attachment {
        Attachment {
            media_type: media_type.to_string(),
            data: None,
            url: None,
            name: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn lexical_vector_is_deterministic_and_case_insensitive() {
        let first = lexical_vector("Semantic routing ranks targets");
        let second = lexical_vector("Semantic routing ranks targets");
        let lower = lexical_vector("semantic routing ranks targets");
        assert_eq!(first, second);
        assert_eq!(first, lower);
        assert_eq!(first.len(), VECTOR_DIMS);
    }

    #[test]
    fn lexical_vector_is_l2_normalized() {
        let vector = lexical_vector("semantic routing");
        assert!((norm(&vector) - 1.0).abs() < 1e-5);
        assert!(lexical_vector("").iter().all(|value| *value == 0.0));
        assert!(lexical_vector("   ").iter().all(|value| *value == 0.0));
    }

    #[test]
    fn cosine_similarity_handles_mismatched_vectors() {
        let vector = lexical_vector("semantic routing");
        assert!((cosine_similarity(&vector, &vector) - 1.0).abs() < 1e-5);
        assert_eq!(cosine_similarity(&vector, &[]), 0.0);
        assert_eq!(cosine_similarity(&vector, &[1.0]), 0.0);
    }

    #[test]
    fn request_text_includes_prompt_text_and_skips_structured_payloads() {
        let mut request = ChatRequest::new("m", vec![Message::user("hello world")]);
        request.system = SystemPrompt::text("system rules");

        let mut assistant = Message::new(Role::Assistant);
        assistant.assistant.push(AssistantContent::Text {
            text: "assistant answer".to_string(),
        });
        assistant.assistant.push(AssistantContent::ToolCall {
            id: "call-1".to_string(),
            name: "lookup".to_string(),
            arguments: json!({"secret": "tool call arguments"}),
            kind: None,
        });
        request.messages.push(assistant);

        let mut tool = Message::new(Role::Tool);
        tool.tool_results.push(ToolResult {
            tool_call_id: "call-1".to_string(),
            name: None,
            content: UserContent::from_parts(vec![
                ContentPart::text("tool result text"),
                ContentPart::Image(attachment("image/png")),
                ContentPart::File(attachment("application/pdf")),
            ]),
            is_error: false,
        });
        request.messages.push(tool);

        let text = request_text(&request, 8_000);
        assert!(text.contains("system rules"));
        assert!(text.contains("hello world"));
        assert!(text.contains("assistant answer"));
        assert!(text.contains("tool result text"));
        assert!(!text.contains("tool call arguments"));
        assert!(!text.contains("image/png"));
        assert!(!text.contains("application/pdf"));
    }

    #[test]
    fn request_text_truncates_by_characters() {
        let request = ChatRequest::new("m", vec![Message::user("你好世界")]);
        assert_eq!(request_text(&request, 2), "你好");
        assert_eq!(request_text(&request, 8_000), "你好世界");
        assert_eq!(request_text(&request, 0), "");
    }
}
