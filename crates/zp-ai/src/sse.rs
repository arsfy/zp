//! Incremental Server-Sent Events decoder.

/// One dispatched SSE event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if present.
    pub event: Option<String>,
    /// All `data:` lines joined with `\n`.
    pub data: String,
}

/// Feeds raw bytes and yields complete events. Handles `\n`, `\r\n` and `\r` line endings
/// and chunks that split lines or multi-byte characters.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    /// Creates an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a chunk and returns the events it completes.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < self.buffer.len() {
            match self.buffer[index] {
                b'\n' => {
                    let line = self.buffer[start..index].to_vec();
                    self.process_line(&line, &mut events);
                    index += 1;
                    start = index;
                }
                b'\r' => {
                    // A trailing `\r` may be the first half of `\r\n`; wait for more input.
                    if index + 1 == self.buffer.len() {
                        break;
                    }
                    let line = self.buffer[start..index].to_vec();
                    self.process_line(&line, &mut events);
                    index += if self.buffer[index + 1] == b'\n' {
                        2
                    } else {
                        1
                    };
                    start = index;
                }
                _ => index += 1,
            }
        }
        self.buffer.drain(..start);
        events
    }

    /// Flushes a final event that was not followed by a blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let mut events = Vec::new();
            let line = line.strip_suffix(b"\r").unwrap_or(&line).to_vec();
            self.process_line(&line, &mut events);
            if let Some(event) = events.pop() {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            events.extend(self.dispatch());
            return;
        }
        if line[0] == b':' {
            return;
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.find(':') {
            Some(position) => {
                let value = &line[position + 1..];
                (&line[..position], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn parses_named_events_and_multiline_data() {
        let mut decoder = SseDecoder::new();
        let events =
            decoder.push(b"event: ping\ndata: {}\n\n: comment\ndata: a\ndata: b\n\nevent: x\n");
        assert_eq!(events, vec![event(Some("ping"), "{}"), event(None, "a\nb")]);
        assert_eq!(decoder.finish(), Some(event(Some("x"), "")));
    }

    #[test]
    fn handles_crlf_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: one\r").is_empty());
        assert!(decoder.push(b"\n").is_empty());
        assert_eq!(decoder.push(b"\r\n"), vec![event(None, "one")]);
    }

    #[test]
    fn handles_bare_cr_and_split_utf8() {
        let mut decoder = SseDecoder::new();
        let bytes = "data: héllo\r\rdata: x".as_bytes();
        let (a, b) = bytes.split_at(8); // splits inside "é"
        let mut events = decoder.push(a);
        events.extend(decoder.push(b));
        assert_eq!(events, vec![event(None, "héllo")]);
        assert_eq!(decoder.finish(), Some(event(None, "x")));
    }

    #[test]
    fn finish_without_pending_event() {
        let mut decoder = SseDecoder::new();
        assert_eq!(decoder.push(b"data: a\n\n"), vec![event(None, "a")]);
        assert_eq!(decoder.finish(), None);
    }
}
