//! Collecting command output: sanitized text, a bounded in-memory tail, and a full copy on
//! disk once it gets large.

use std::io::Write as _;
use std::path::PathBuf;

use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Truncated, truncate_tail};

/// Raw bytes kept in memory before spilling to a file.
const MEMORY_LIMIT: usize = 1024 * 1024;
/// Tail kept in memory once spilled.
const TAIL_BYTES: usize = 4 * DEFAULT_MAX_BYTES;

/// Accumulates raw output.
pub(crate) struct OutputBuffer {
    buffer: Vec<u8>,
    total: usize,
    file: Option<(PathBuf, std::fs::File)>,
}

impl OutputBuffer {
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            total: 0,
            file: None,
        }
    }

    /// Appends a chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        if let Some((_, file)) = &mut self.file {
            let _ = file.write_all(chunk);
        }
        self.buffer.extend_from_slice(chunk);
        if self.file.is_none() && self.buffer.len() > MEMORY_LIMIT {
            self.spill();
        }
        if self.file.is_some() && self.buffer.len() > 2 * TAIL_BYTES {
            let cut = self.buffer.len() - TAIL_BYTES;
            self.buffer.drain(..cut);
        }
    }

    /// Moves everything so far into a temp file; later chunks are appended there too.
    fn spill(&mut self) {
        let path = std::env::temp_dir().join(format!("zp-bash-{}.log", uuid::Uuid::new_v4()));
        match std::fs::File::create(&path) {
            Ok(mut file) => {
                let _ = file.write_all(&self.buffer);
                self.file = Some((path, file));
            }
            Err(error) => tracing::warn!(%error, "could not create output file"),
        }
    }

    /// The sanitized tail for display, truncated to the output limits, plus the path of
    /// the full output when it was truncated.
    pub(crate) fn snapshot(&mut self, persist: bool) -> (Truncated, Option<PathBuf>) {
        let text = sanitize(&String::from_utf8_lossy(&self.buffer));
        let cut = truncate_tail(&text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let truncated = cut.info.truncated || self.total > self.buffer.len();
        if truncated && persist && self.file.is_none() {
            self.spill();
        }
        let path = if truncated {
            self.file.as_ref().map(|(path, _)| path.clone())
        } else {
            None
        };
        (cut, path)
    }

    /// Whether nothing was written.
    pub(crate) fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// Makes terminal output readable as plain text: drops ANSI escape sequences and control
/// characters, and resolves carriage-return redraws (progress bars) to their final state.
pub(crate) fn sanitize(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                // CSI: ESC [ params final-byte
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... (BEL | ESC \)
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\n' | '\t' | '\r' => plain.push(c),
            c if c.is_control() => {}
            '\u{FFF9}'..='\u{FFFB}' => {}
            c => plain.push(c),
        }
    }
    plain
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            // After a bare CR the terminal redraws the line; keep the last state.
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_terminal_output() {
        assert_eq!(
            sanitize("\x1b[31mred\x1b[0m\r\n10%\r50%\r100%\nbell\x07\x1b]0;title\x07done"),
            "red\n100%\nbelldone"
        );
    }

    #[test]
    fn spills_large_output_and_keeps_the_tail() {
        let mut output = OutputBuffer::new();
        let line = "x".repeat(99) + "\n";
        for _ in 0..30_000 {
            output.push(line.as_bytes());
        }
        let (cut, path) = output.snapshot(true);
        assert!(cut.info.truncated);
        let path = path.expect("full output on disk");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 3_000_000);
        assert!(cut.content.ends_with(&"x".repeat(99)));
        std::fs::remove_file(path).unwrap();

        let mut small = OutputBuffer::new();
        small.push(b"ok\n");
        let (cut, path) = small.snapshot(true);
        assert_eq!((cut.content.as_str(), path), ("ok\n", None));
    }
}
