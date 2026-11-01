//! Incremental Server-Sent Events decoder shared by the LLM and MCP gateways.
//!
//! The decoder is transport agnostic: feed it response body chunks and it
//! yields complete events whenever a blank-line boundary is reached.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a chunk and returns every event completed by this chunk.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some((end, separator)) = find_boundary(&self.buffer) {
            let block = self.buffer[..end].to_vec();
            self.buffer.drain(..end + separator);
            if let Some(event) = parse_block(&block) {
                events.push(event);
            }
        }

        events
    }

    /// Flushes a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buffer.is_empty() {
            return None;
        }
        let block = std::mem::take(&mut self.buffer);
        parse_block(&block)
    }
}

fn find_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index] == b'\n' {
            if buffer.get(index + 1) == Some(&b'\n') {
                return Some((index, 2));
            }
            if buffer.get(index + 1) == Some(&b'\r') && buffer.get(index + 2) == Some(&b'\n') {
                return Some((index, 3));
            }
        }
        index += 1;
    }
    None
}

fn parse_block(block: &[u8]) -> Option<SseEvent> {
    let text = std::str::from_utf8(block).ok()?;
    let mut event = None;
    let mut data = Vec::new();
    let mut id = None;
    let mut retry = None;

    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            "id" => id = Some(value.to_string()),
            "retry" => retry = value.parse().ok(),
            _ => {}
        }
    }

    if data.is_empty() {
        return None;
    }

    Some(SseEvent {
        event,
        data: data.join("\n"),
        id,
        retry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_events_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"event: delta\nda").is_empty());
        let events = decoder.push(b"ta: hello\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn decodes_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b"id: 7\r\ndata: one\r\ndata: two\r\n\r\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one\ntwo");
        assert_eq!(events[0].id.as_deref(), Some("7"));
    }

    #[test]
    fn ignores_comments_and_empty_events() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b": keepalive\n\ndata: real\n\n");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn finishes_trailing_event_without_blank_line() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: tail").is_empty());

        let event = decoder.finish().expect("trailing event");
        assert_eq!(event.data, "tail");
    }
}
