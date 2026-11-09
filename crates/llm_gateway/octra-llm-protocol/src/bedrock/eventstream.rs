//! Incremental decoder for the AWS event-stream framing used by
//! `converse-stream`.
//!
//! Each message is a big-endian prelude (total length, headers length, prelude
//! CRC32), the headers, the payload and a trailing message CRC32. The decoder
//! validates both CRCs, buffers partial messages and yields complete frames.
//! Only the `:event-type` and `:exception-type` string headers are retained;
//! every other header is skipped by its documented width.

const PRELUDE_LEN: usize = 12;
const MIN_TOTAL_LEN: usize = 16;
const MAX_TOTAL_LEN: usize = 16 * 1024 * 1024;

/// One decoded event-stream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFrame {
    pub event_type: String,
    pub payload: String,
}

/// Buffers partial event-stream messages and returns complete frames.
#[derive(Debug, Default)]
pub struct Framer {
    buffer: Vec<u8>,
}

impl Framer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a chunk and returns every frame completed by this chunk.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<EventFrame>, String> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();

        loop {
            if self.buffer.len() < PRELUDE_LEN {
                break;
            }
            let total_len = u32::from_be_bytes(
                self.buffer[0..4]
                    .try_into()
                    .expect("prelude length checked"),
            ) as usize;
            let headers_len = u32::from_be_bytes(
                self.buffer[4..8]
                    .try_into()
                    .expect("prelude length checked"),
            ) as usize;

            if total_len < MIN_TOTAL_LEN {
                return Err(format!(
                    "event-stream frame length {total_len} is smaller than {MIN_TOTAL_LEN}"
                ));
            }
            if total_len > MAX_TOTAL_LEN {
                return Err(format!(
                    "event-stream frame length {total_len} exceeds the {MAX_TOTAL_LEN}-byte limit"
                ));
            }
            if headers_len > total_len - MIN_TOTAL_LEN {
                return Err(format!(
                    "event-stream headers length {headers_len} does not fit in a {total_len}-byte frame"
                ));
            }

            let prelude_crc = u32::from_be_bytes(
                self.buffer[8..12]
                    .try_into()
                    .expect("prelude length checked"),
            );
            let computed = crc32(&self.buffer[..8]);
            if prelude_crc != computed {
                return Err(format!(
                    "event-stream prelude CRC mismatch: frame says {prelude_crc:08x}, computed {computed:08x}"
                ));
            }

            if self.buffer.len() < total_len {
                break;
            }

            let message_crc = u32::from_be_bytes(
                self.buffer[total_len - 4..total_len]
                    .try_into()
                    .expect("frame length checked"),
            );
            let computed = crc32(&self.buffer[..total_len - 4]);
            if message_crc != computed {
                return Err(format!(
                    "event-stream message CRC mismatch: frame says {message_crc:08x}, computed {computed:08x}"
                ));
            }

            let headers_start = PRELUDE_LEN;
            let headers_end = headers_start + headers_len;
            let payload_end = total_len - 4;
            if headers_end > payload_end {
                return Err("event-stream headers overlap the message CRC".to_string());
            }
            let (event_type, exception_type) =
                parse_headers(&self.buffer[headers_start..headers_end])?;
            let payload =
                String::from_utf8_lossy(&self.buffer[headers_end..payload_end]).into_owned();
            frames.push(EventFrame {
                event_type: event_type
                    .or(exception_type)
                    .unwrap_or_else(|| "unknown".to_string()),
                payload,
            });
            self.buffer.drain(..total_len);
        }

        Ok(frames)
    }
}

fn parse_headers(mut bytes: &[u8]) -> Result<(Option<String>, Option<String>), String> {
    let mut event_type = None;
    let mut exception_type = None;
    while !bytes.is_empty() {
        let name_len = bytes[0] as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len {
            return Err("event-stream header name is truncated".to_string());
        }
        let name = std::str::from_utf8(&bytes[..name_len])
            .map_err(|_| "event-stream header name is not UTF-8".to_string())?;
        bytes = &bytes[name_len..];
        let Some((&value_type, rest)) = bytes.split_first() else {
            return Err("event-stream header value type is missing".to_string());
        };
        bytes = rest;
        let value = read_header_value(value_type, &mut bytes)?;
        match name {
            ":event-type" => event_type = value,
            ":exception-type" => exception_type = value,
            _ => {}
        }
    }
    Ok((event_type, exception_type))
}

fn read_header_value(value_type: u8, bytes: &mut &[u8]) -> Result<Option<String>, String> {
    match value_type {
        0 | 1 => Ok(None),
        2 => {
            take(bytes, 1)?;
            Ok(None)
        }
        3 => {
            take(bytes, 2)?;
            Ok(None)
        }
        4 => {
            take(bytes, 4)?;
            Ok(None)
        }
        5 => {
            take(bytes, 8)?;
            Ok(None)
        }
        6 => {
            let len = read_u16(bytes)? as usize;
            take(bytes, len)?;
            Ok(None)
        }
        7 => {
            let len = read_u16(bytes)? as usize;
            let value = take(bytes, len)?;
            std::str::from_utf8(value)
                .map(|value| Some(value.to_string()))
                .map_err(|_| "event-stream string header is not UTF-8".to_string())
        }
        8 => {
            take(bytes, 8)?;
            Ok(None)
        }
        9 => {
            take(bytes, 16)?;
            Ok(None)
        }
        other => Err(format!(
            "unsupported event-stream header value type {other}"
        )),
    }
}

fn read_u16(bytes: &mut &[u8]) -> Result<u16, String> {
    let value = take(bytes, 2)?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], String> {
    if bytes.len() < len {
        return Err("event-stream header value is truncated".to_string());
    }
    let (head, tail) = bytes.split_at(len);
    *bytes = tail;
    Ok(head)
}

/// IEEE CRC32 (polynomial `0xEDB88320`), computed with a const table.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC_TABLE[index];
    }
    !crc
}

const CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.push(":event-type".len() as u8);
        headers.extend_from_slice(b":event-type");
        headers.push(7);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());
        build_frame(&headers, payload.as_bytes())
    }

    fn build_frame(headers: &[u8], payload: &[u8]) -> Vec<u8> {
        let total = PRELUDE_LEN + headers.len() + payload.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&out[..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(headers);
        out.extend_from_slice(payload);
        let message_crc = crc32(&out);
        out.extend_from_slice(&message_crc.to_be_bytes());
        out
    }

    #[test]
    fn crc32_matches_the_ieee_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn parses_one_complete_frame() {
        let mut framer = Framer::new();
        let frames = framer
            .push(&frame("messageStart", r#"{"role":"assistant"}"#))
            .unwrap();
        assert_eq!(
            frames,
            vec![EventFrame {
                event_type: "messageStart".to_string(),
                payload: r#"{"role":"assistant"}"#.to_string(),
            }]
        );
    }

    #[test]
    fn parses_two_frames_in_one_buffer() {
        let mut bytes = frame("contentBlockDelta", r#"{"contentBlockIndex":0}"#);
        bytes.extend(frame("messageStop", r#"{"stopReason":"end_turn"}"#));
        let frames = Framer::new().push(&bytes).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event_type, "contentBlockDelta");
        assert_eq!(frames[1].event_type, "messageStop");
    }

    #[test]
    fn parses_a_frame_split_across_pushes() {
        let bytes = frame("contentBlockStop", r#"{"contentBlockIndex":0}"#);
        let (first, second) = bytes.split_at(bytes.len() / 2);
        let mut framer = Framer::new();
        assert!(framer.push(first).unwrap().is_empty());
        let frames = framer.push(second).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event_type, "contentBlockStop");
    }

    #[test]
    fn rejects_a_bad_message_crc() {
        let mut bytes = frame("messageStop", r#"{"stopReason":"end_turn"}"#);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let error = Framer::new().push(&bytes).unwrap_err();
        assert!(error.contains("message CRC mismatch"), "{error}");
    }

    #[test]
    fn rejects_an_oversized_frame() {
        let total = (MAX_TOTAL_LEN + 1) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&total.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let crc = crc32(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        let error = Framer::new().push(&bytes).unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
    }

    #[test]
    fn falls_back_to_exception_type_then_unknown() {
        let mut headers = Vec::new();
        headers.push(":exception-type".len() as u8);
        headers.extend_from_slice(b":exception-type");
        headers.push(7);
        headers.extend_from_slice(&(15u16).to_be_bytes());
        headers.extend_from_slice(b"validationError");
        let frames = Framer::new().push(&build_frame(&headers, b"{}")).unwrap();
        assert_eq!(frames[0].event_type, "validationError");

        let frames = Framer::new().push(&build_frame(&[], b"{}")).unwrap();
        assert_eq!(frames[0].event_type, "unknown");
    }
}
