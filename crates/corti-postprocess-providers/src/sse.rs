use std::str;

use corti_postprocess::{ErrorCode, PostprocessError};

const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental, bounded SSE decoder that accepts LF, CRLF, and CR line endings and arbitrary byte chunk
/// boundaries (including boundaries inside UTF-8 code points).
pub(crate) struct SseDecoder {
    pending_line: Vec<u8>,
    event_lines: Vec<Vec<u8>>,
    event_bytes: usize,
    total_bytes: usize,
    max_total_bytes: usize,
}

impl SseDecoder {
    pub fn new(max_total_bytes: usize) -> Self {
        Self {
            pending_line: Vec::new(),
            event_lines: Vec::new(),
            event_bytes: 0,
            total_bytes: 0,
            max_total_bytes,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, PostprocessError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes.len())
            .ok_or_else(malformed)?;
        if self.total_bytes > self.max_total_bytes {
            return Err(malformed());
        }
        self.pending_line.extend_from_slice(bytes);
        self.consume_complete_lines(false)
    }

    pub fn finish(&mut self) -> Result<Vec<SseEvent>, PostprocessError> {
        let mut events = self.consume_complete_lines(true)?;
        if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            self.push_line(line)?;
        }
        if let Some(event) = self.dispatch()? {
            events.push(event);
        }
        Ok(events)
    }

    fn consume_complete_lines(
        &mut self,
        final_input: bool,
    ) -> Result<Vec<SseEvent>, PostprocessError> {
        let mut events = Vec::new();
        while let Some((line_end, terminator_bytes)) =
            next_line_end(&self.pending_line, final_input)
        {
            let line = self.pending_line[..line_end].to_vec();
            self.pending_line.drain(..line_end + terminator_bytes);
            if line.is_empty() {
                if let Some(event) = self.dispatch()? {
                    events.push(event);
                }
            } else {
                self.push_line(line)?;
            }
        }
        Ok(events)
    }

    fn push_line(&mut self, line: Vec<u8>) -> Result<(), PostprocessError> {
        self.event_bytes = self
            .event_bytes
            .checked_add(line.len())
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or_else(malformed)?;
        if self.event_bytes > MAX_SSE_EVENT_BYTES {
            return Err(malformed());
        }
        self.event_lines.push(line);
        Ok(())
    }

    fn dispatch(&mut self) -> Result<Option<SseEvent>, PostprocessError> {
        let lines = std::mem::take(&mut self.event_lines);
        self.event_bytes = 0;
        if lines.is_empty() {
            return Ok(None);
        }

        let mut event = None;
        let mut data = Vec::new();
        for line in lines {
            let line = str::from_utf8(&line).map_err(|_| malformed())?;
            if line.starts_with(':') {
                continue;
            }
            let (field, mut value) = line.split_once(':').unwrap_or((line, ""));
            if let Some(stripped) = value.strip_prefix(' ') {
                value = stripped;
            }
            match field {
                "event" => {
                    if value.contains('\0') {
                        return Err(malformed());
                    }
                    event = Some(value.to_owned());
                }
                "data" => data.push(value.to_owned()),
                // `id`, `retry`, and extension fields do not affect provider event decoding.
                _ => {}
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(SseEvent {
            event,
            data: data.join("\n"),
        }))
    }
}

fn next_line_end(bytes: &[u8], final_input: bool) -> Option<(usize, usize)> {
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'\n' => return Some((index, 1)),
            b'\r' => {
                if bytes.get(index + 1) == Some(&b'\n') {
                    return Some((index, 2));
                }
                if index + 1 < bytes.len() || final_input {
                    return Some((index, 1));
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

fn malformed() -> PostprocessError {
    ErrorCode::MalformedOutput.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_chunked_utf8_and_mixed_line_endings() {
        let mut wire = b"event: delta\r\ndata: {\"text\":\"caf".to_vec();
        wire.extend_from_slice("é\"}\n\n".as_bytes());
        let split = wire.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
        let mut decoder = SseDecoder::new(4096);
        assert!(decoder.push(&wire[..split]).unwrap().is_empty());
        let events = decoder.push(&wire[split..]).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert!(events[0].data.contains("café"));
    }

    #[test]
    fn joins_multiple_data_lines_and_ignores_comments() {
        let mut decoder = SseDecoder::new(4096);
        let events = decoder
            .push(b": keepalive\ndata: first\ndata: second\n\n")
            .unwrap();
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[test]
    fn rejects_invalid_utf8_without_echoing_it() {
        let mut decoder = SseDecoder::new(4096);
        let error = decoder.push(b"data: \xff\n\n").unwrap_err();
        assert_eq!(error.code, ErrorCode::MalformedOutput);
    }
}
