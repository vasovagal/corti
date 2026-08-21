//! `application/vnd.amazon.eventstream` frame decoding.
//!
//! Bedrock's `ConverseStream` replies in AWS's binary framing rather than SSE. Frames arrive split
//! across transport chunks, so the decoder buffers until a whole message — prelude, prelude CRC,
//! headers, payload, message CRC — is present. A truncated or CRC-corrupt frame is
//! [`ErrorCode::MalformedOutput`]; it is never a panic and never a silent skip.

use std::fmt;

use corti_postprocess::{ErrorCode, PostprocessError};

const PRELUDE_BYTES: usize = 12;
const MESSAGE_OVERHEAD_BYTES: usize = 16;
/// AWS's documented ceiling for one event-stream message.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 128 * 1024;

/// One decoded message. Only string-valued headers are projected; other header types are parsed to
/// advance the cursor correctly but are not exposed, because no Bedrock event needs them.
pub(crate) struct EventStreamMessage {
    headers: Vec<(String, String)>,
    payload: Vec<u8>,
}

impl EventStreamMessage {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn message_type(&self) -> Option<&str> {
        self.header(":message-type")
    }

    pub(crate) fn event_type(&self) -> Option<&str> {
        self.header(":event-type")
    }

    pub(crate) fn exception_type(&self) -> Option<&str> {
        self.header(":exception-type")
    }

    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl fmt::Debug for EventStreamMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header names are protocol constants; values and payload are provider content.
        f.debug_struct("EventStreamMessage")
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

pub(crate) struct EventStreamDecoder {
    buffer: Vec<u8>,
    max_stream_bytes: usize,
    consumed: usize,
}

impl EventStreamDecoder {
    pub(crate) fn new(max_stream_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_stream_bytes,
            consumed: 0,
        }
    }

    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<EventStreamMessage>, PostprocessError> {
        self.consumed = self
            .consumed
            .checked_add(chunk.len())
            .ok_or(PostprocessError::from(ErrorCode::MalformedOutput))?;
        if self.consumed > self.max_stream_bytes {
            return Err(ErrorCode::MalformedOutput.into());
        }
        self.buffer.extend_from_slice(chunk);

        let mut messages = Vec::new();
        loop {
            if self.buffer.len() < PRELUDE_BYTES {
                break;
            }
            let total_length = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
            let headers_length = u32::from_be_bytes(self.buffer[4..8].try_into().unwrap()) as usize;
            if !(MESSAGE_OVERHEAD_BYTES..=MAX_MESSAGE_BYTES).contains(&total_length)
                || headers_length > MAX_HEADER_BYTES
                || headers_length > total_length - MESSAGE_OVERHEAD_BYTES
            {
                return Err(ErrorCode::MalformedOutput.into());
            }
            let prelude_crc = u32::from_be_bytes(self.buffer[8..12].try_into().unwrap());
            if crc32(&self.buffer[0..8]) != prelude_crc {
                return Err(ErrorCode::MalformedOutput.into());
            }
            if self.buffer.len() < total_length {
                break;
            }
            let message_crc = u32::from_be_bytes(
                self.buffer[total_length - 4..total_length]
                    .try_into()
                    .unwrap(),
            );
            if crc32(&self.buffer[0..total_length - 4]) != message_crc {
                return Err(ErrorCode::MalformedOutput.into());
            }
            let headers_end = PRELUDE_BYTES + headers_length;
            let headers = decode_headers(&self.buffer[PRELUDE_BYTES..headers_end])?;
            let payload = self.buffer[headers_end..total_length - 4].to_vec();
            messages.push(EventStreamMessage { headers, payload });
            self.buffer.drain(0..total_length);
        }
        Ok(messages)
    }

    /// A stream that ends mid-frame is malformed; partial trailing bytes are never handed back.
    pub(crate) fn finish(&self) -> Result<(), PostprocessError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ErrorCode::MalformedOutput.into())
        }
    }
}

impl fmt::Debug for EventStreamDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStreamDecoder")
            .field("buffered_bytes", &self.buffer.len())
            .field("consumed_bytes", &self.consumed)
            .field("max_stream_bytes", &self.max_stream_bytes)
            .finish()
    }
}

fn decode_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, PostprocessError> {
    let malformed = || PostprocessError::from(ErrorCode::MalformedOutput);
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let name_len = *bytes.first().ok_or_else(malformed)? as usize;
        bytes = bytes.get(1..).ok_or_else(malformed)?;
        let name = std::str::from_utf8(bytes.get(..name_len).ok_or_else(malformed)?)
            .map_err(|_| malformed())?
            .to_owned();
        bytes = bytes.get(name_len..).ok_or_else(malformed)?;
        let value_type = *bytes.first().ok_or_else(malformed)?;
        bytes = bytes.get(1..).ok_or_else(malformed)?;
        // Fixed-width types are skipped; only strings are projected, but every width must be known
        // or the cursor desynchronizes and later headers decode as garbage.
        let fixed = match value_type {
            0 | 1 => Some(0),
            2 => Some(1),
            3 => Some(2),
            4 => Some(4),
            5 | 8 => Some(8),
            9 => Some(16),
            6 | 7 => None,
            _ => return Err(malformed()),
        };
        if let Some(width) = fixed {
            bytes = bytes.get(width..).ok_or_else(malformed)?;
            continue;
        }
        let length = u16::from_be_bytes(
            bytes
                .get(..2)
                .ok_or_else(malformed)?
                .try_into()
                .map_err(|_| malformed())?,
        ) as usize;
        bytes = bytes.get(2..).ok_or_else(malformed)?;
        let value = bytes.get(..length).ok_or_else(malformed)?;
        if value_type == 7 {
            headers.push((
                name,
                std::str::from_utf8(value)
                    .map_err(|_| malformed())?
                    .to_owned(),
            ));
        }
        bytes = bytes.get(length..).ok_or_else(malformed)?;
    }
    Ok(headers)
}

/// CRC-32 (IEEE 802.3, reflected), the checksum AWS uses for both the prelude and the whole message.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a well-formed message with only string headers.
    fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7);
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }
        let total = MESSAGE_OVERHEAD_BYTES + header_bytes.len() + payload.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&out[0..8]);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(payload);
        let message_crc = crc32(&out);
        out.extend_from_slice(&message_crc.to_be_bytes());
        out
    }

    #[test]
    fn crc32_matches_the_reference_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn decodes_a_complete_message() {
        let bytes = frame(
            &[
                (":message-type", "event"),
                (":event-type", "contentBlockDelta"),
            ],
            br#"{"delta":{"text":"hi"}}"#,
        );
        let mut decoder = EventStreamDecoder::new(1024 * 1024);
        let messages = decoder.push(&bytes).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message_type(), Some("event"));
        assert_eq!(messages[0].event_type(), Some("contentBlockDelta"));
        assert_eq!(messages[0].payload(), br#"{"delta":{"text":"hi"}}"#);
        decoder.finish().unwrap();
    }

    #[test]
    fn reassembles_frames_split_across_every_byte_boundary() {
        let first = frame(&[(":event-type", "contentBlockDelta")], b"{\"a\":1}");
        let second = frame(&[(":event-type", "metadata")], b"{\"b\":2}");
        let stream = [first, second].concat();
        for split in 1..stream.len() {
            let mut decoder = EventStreamDecoder::new(1024 * 1024);
            let mut decoded = decoder.push(&stream[..split]).unwrap();
            decoded.extend(decoder.push(&stream[split..]).unwrap());
            assert_eq!(decoded.len(), 2, "split at {split}");
            assert_eq!(decoded[0].event_type(), Some("contentBlockDelta"));
            assert_eq!(decoded[1].event_type(), Some("metadata"));
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn one_byte_at_a_time_still_decodes() {
        let bytes = frame(&[(":event-type", "metadata")], b"{}");
        let mut decoder = EventStreamDecoder::new(1024 * 1024);
        let mut decoded = Vec::new();
        for byte in &bytes {
            decoded.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(decoded.len(), 1);
        decoder.finish().unwrap();
    }

    #[test]
    fn truncated_stream_fails_closed_rather_than_yielding_a_partial_event() {
        let bytes = frame(&[(":event-type", "metadata")], b"{}");
        for truncate in 1..bytes.len() {
            let mut decoder = EventStreamDecoder::new(1024 * 1024);
            assert!(decoder.push(&bytes[..truncate]).unwrap().is_empty());
            assert_eq!(
                decoder.finish().unwrap_err().code,
                ErrorCode::MalformedOutput,
                "truncated at {truncate}"
            );
        }
    }

    #[test]
    fn corrupting_any_byte_is_malformed_output_and_never_a_panic() {
        let clean = frame(&[(":event-type", "contentBlockDelta")], b"{\"text\":\"x\"}");
        for index in 0..clean.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut bytes = clean.clone();
                bytes[index] ^= flip;
                if bytes == clean {
                    continue;
                }
                let mut decoder = EventStreamDecoder::new(1024 * 1024);
                let outcome = decoder.push(&bytes).and_then(|messages| {
                    decoder.finish()?;
                    Ok(messages)
                });
                match outcome {
                    Ok(messages) => {
                        // A CRC collision is not expected, but if one occurs the frame must at least
                        // still be internally consistent rather than a partially applied decode.
                        assert!(messages.len() <= 1);
                    }
                    Err(error) => assert_eq!(error.code, ErrorCode::MalformedOutput),
                }
            }
        }
    }

    #[test]
    fn oversized_and_inconsistent_preludes_are_rejected() {
        let mut bytes = frame(&[(":event-type", "metadata")], b"{}");
        // headers_length larger than the message can hold.
        bytes[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        let prelude_crc = crc32(&bytes[0..8]).to_be_bytes();
        bytes[8..12].copy_from_slice(&prelude_crc);
        let mut decoder = EventStreamDecoder::new(1024 * 1024);
        assert_eq!(
            decoder.push(&bytes).unwrap_err().code,
            ErrorCode::MalformedOutput
        );
    }

    #[test]
    fn stream_byte_budget_is_enforced() {
        let bytes = frame(&[(":event-type", "metadata")], &vec![b'x'; 512]);
        let mut decoder = EventStreamDecoder::new(64);
        assert_eq!(
            decoder.push(&bytes).unwrap_err().code,
            ErrorCode::MalformedOutput
        );
    }

    #[test]
    fn non_string_header_types_advance_the_cursor_without_being_projected() {
        let mut header_bytes = Vec::new();
        // A boolean header, then a long, then the string we actually read.
        header_bytes.extend_from_slice(&[4, b'b', b'o', b'o', b'l', 0]);
        header_bytes.extend_from_slice(&[4, b'l', b'o', b'n', b'g', 5]);
        header_bytes.extend_from_slice(&0i64.to_be_bytes());
        header_bytes.push(11);
        header_bytes.extend_from_slice(b":event-type");
        header_bytes.push(7);
        header_bytes.extend_from_slice(&8u16.to_be_bytes());
        header_bytes.extend_from_slice(b"metadata");

        let payload = b"{}";
        let total = MESSAGE_OVERHEAD_BYTES + header_bytes.len() + payload.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(total as u32).to_be_bytes());
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&crc32(&bytes[0..8]).to_be_bytes());
        bytes.extend_from_slice(&header_bytes);
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());

        let mut decoder = EventStreamDecoder::new(1024 * 1024);
        let messages = decoder.push(&bytes).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].event_type(), Some("metadata"));
        assert_eq!(messages[0].header("bool"), None);
        assert_eq!(messages[0].header("long"), None);
    }

    #[test]
    fn debug_never_renders_payload_or_header_values() {
        let bytes = frame(
            &[(":event-type", "synthetic-header-value")],
            b"synthetic-payload-never-render",
        );
        let mut decoder = EventStreamDecoder::new(1024 * 1024);
        let messages = decoder.push(&bytes).unwrap();
        let rendered = format!("{:?}", messages[0]);
        assert!(!rendered.contains("synthetic-payload"));
        assert!(!rendered.contains("synthetic-header-value"));
        assert!(rendered.contains(":event-type"));
    }
}
