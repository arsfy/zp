//! Output truncation shared by the tools.
//!
//! Two independent limits apply (whichever is hit first): a line count and a byte size.
//! Whole lines are kept, except when a single trailing line alone exceeds the byte limit in
//! tail mode.

use serde::{Deserialize, Serialize};

/// Default line limit for tool output.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// Default byte limit for tool output (50 KB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Longest line shown in grep results.
pub const GREP_MAX_LINE_CHARS: usize = 500;

/// Which limit cut the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    /// The line limit.
    Lines,
    /// The byte limit.
    Bytes,
}

/// What truncation did, reported in tool `details`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationInfo {
    /// Whether anything was dropped.
    pub truncated: bool,
    /// The limit that applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_by: Option<TruncatedBy>,
    /// Lines in the input.
    pub total_lines: usize,
    /// Bytes in the input.
    pub total_bytes: usize,
    /// Lines kept.
    pub output_lines: usize,
    /// Bytes kept.
    pub output_bytes: usize,
    /// Tail mode only: the first kept line is the end of a longer line.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_line_partial: bool,
    /// Head mode only: the first line alone exceeds the byte limit, so nothing is kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first_line_exceeds_limit: bool,
}

/// Kept text plus what was dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncated {
    /// The kept text.
    pub content: String,
    /// Truncation report.
    pub info: TruncationInfo,
}

/// Lines for counting: a trailing newline does not start another line.
fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn untouched(content: &str, lines: usize) -> Truncated {
    Truncated {
        content: content.to_owned(),
        info: TruncationInfo {
            total_lines: lines,
            total_bytes: content.len(),
            output_lines: lines,
            output_bytes: content.len(),
            ..TruncationInfo::default()
        },
    }
}

/// Keeps the first lines (for file reads).
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines = split_lines(content);
    if lines.len() <= max_lines && content.len() <= max_bytes {
        return untouched(content, lines.len());
    }
    let mut info = TruncationInfo {
        truncated: true,
        total_lines: lines.len(),
        total_bytes: content.len(),
        ..TruncationInfo::default()
    };
    if lines.first().is_some_and(|line| line.len() > max_bytes) {
        info.truncated_by = Some(TruncatedBy::Bytes);
        info.first_line_exceeds_limit = true;
        return Truncated {
            content: String::new(),
            info,
        };
    }
    let mut kept = 0;
    let mut bytes = 0;
    let mut by = TruncatedBy::Lines;
    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let size = line.len() + usize::from(i > 0);
        if bytes + size > max_bytes {
            by = TruncatedBy::Bytes;
            break;
        }
        kept += 1;
        bytes += size;
    }
    let content = lines[..kept].join("\n");
    info.truncated_by = Some(by);
    info.output_lines = kept;
    info.output_bytes = content.len();
    Truncated { content, info }
}

/// Keeps the last lines (for command output, where errors and results come last).
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    let lines = split_lines(content);
    if lines.len() <= max_lines && content.len() <= max_bytes {
        return untouched(content, lines.len());
    }
    let mut info = TruncationInfo {
        truncated: true,
        total_lines: lines.len(),
        total_bytes: content.len(),
        ..TruncationInfo::default()
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut partial = String::new();
    let mut bytes = 0;
    let mut by = TruncatedBy::Lines;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > max_bytes {
            by = TruncatedBy::Bytes;
            if kept.is_empty() {
                partial = tail_bytes(line, max_bytes).to_owned();
                info.last_line_partial = true;
            }
            break;
        }
        kept.push(line);
        bytes += size;
    }
    kept.reverse();
    let content = if info.last_line_partial {
        partial
    } else {
        kept.join("\n")
    };
    info.truncated_by = Some(by);
    info.output_lines = if info.last_line_partial {
        1
    } else {
        kept.len()
    };
    info.output_bytes = content.len();
    Truncated { content, info }
}

/// The longest suffix of `text` within `max_bytes`, on a character boundary.
fn tail_bytes(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Shortens `line` to `max_chars` characters with a marker. Returns whether it was cut.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    match line.char_indices().nth(max_chars) {
        Some((cut, _)) => (format!("{}... [truncated]", &line[..cut]), true),
        None => (line.to_owned(), false),
    }
}

/// Human-readable size: `512B`, `1.5KB`, `2.0MB`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_whole_lines() {
        let text = "a\nbb\nccc\n";
        assert!(!truncate_head(text, 10, 100).info.truncated);

        let cut = truncate_head(text, 2, 100);
        assert_eq!(cut.content, "a\nbb");
        assert_eq!(cut.info.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!((cut.info.total_lines, cut.info.output_lines), (3, 2));

        let cut = truncate_head(text, 10, 5);
        assert_eq!(cut.content, "a\nbb");
        assert_eq!(cut.info.truncated_by, Some(TruncatedBy::Bytes));

        let cut = truncate_head("toolongline\nx", 10, 4);
        assert!(cut.info.first_line_exceeds_limit && cut.content.is_empty());
    }

    #[test]
    fn tail_keeps_the_end() {
        let cut = truncate_tail("1\n2\n3\n4\n", 2, 100);
        assert_eq!(cut.content, "3\n4");
        assert_eq!(cut.info.output_lines, 2);

        let cut = truncate_tail("short\nééééé", 10, 5);
        assert!(cut.info.last_line_partial);
        assert_eq!(cut.content, "éé");
    }

    #[test]
    fn helpers() {
        assert_eq!(
            truncate_line("abcdef", 3),
            ("abc... [truncated]".into(), true)
        );
        assert_eq!(truncate_line("abc", 3), ("abc".into(), false));
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }
}
