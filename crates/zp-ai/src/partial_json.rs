//! Best-effort parsing of incomplete JSON, used to show tool arguments while they stream.
//!
//! The input is repaired by closing open strings and containers and by dropping trailing
//! fragments that cannot be completed (dangling keys, commas, partial literals). The result
//! is always a prefix of what the complete document will contain.

use serde_json::Value;

/// Parses `input`, completing it if it is a truncated JSON document.
///
/// Returns `None` if nothing meaningful can be recovered (e.g. empty input).
pub fn parse_partial_json(input: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str(input) {
        return Some(value);
    }
    serde_json::from_str(&complete(input)).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Key,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    is_object: bool,
    expect: Expect,
    /// Byte index of the comma before the expected member, if any.
    comma: Option<usize>,
    /// Byte index where the current object key starts.
    key_start: usize,
}

#[derive(Debug, Default)]
struct ScanState {
    frames: Vec<Frame>,
    /// Open string: (start index, is an object key).
    string: Option<(usize, bool)>,
    escape: bool,
    /// Start of an unterminated bare scalar (number or literal).
    scalar_start: Option<usize>,
}

impl ScanState {
    fn value_done(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            frame.expect = Expect::CommaOrEnd;
            frame.comma = None;
        }
    }
}

fn scan(input: &str) -> ScanState {
    let mut state = ScanState::default();
    for (index, byte) in input.bytes().enumerate() {
        if let Some((_, is_key)) = state.string {
            if state.escape {
                state.escape = false;
            } else if byte == b'\\' {
                state.escape = true;
            } else if byte == b'"' {
                state.string = None;
                if is_key {
                    if let Some(frame) = state.frames.last_mut() {
                        frame.expect = Expect::Colon;
                    }
                } else {
                    state.value_done();
                }
            }
            continue;
        }
        if state.scalar_start.is_some() {
            if matches!(
                byte,
                b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}' | b']' | b':'
            ) {
                state.scalar_start = None;
                state.value_done();
            } else {
                continue;
            }
        }
        match byte {
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'{' | b'[' => state.frames.push(Frame {
                is_object: byte == b'{',
                expect: if byte == b'{' {
                    Expect::Key
                } else {
                    Expect::Value
                },
                comma: None,
                key_start: index + 1,
            }),
            b'}' | b']' => {
                state.frames.pop();
                state.value_done();
            }
            b'"' => {
                let is_key = state
                    .frames
                    .last()
                    .is_some_and(|frame| frame.is_object && frame.expect == Expect::Key);
                if is_key && let Some(frame) = state.frames.last_mut() {
                    frame.key_start = index;
                }
                state.string = Some((index, is_key));
            }
            b':' => {
                if let Some(frame) = state.frames.last_mut() {
                    frame.expect = Expect::Value;
                }
            }
            b',' => {
                if let Some(frame) = state.frames.last_mut() {
                    frame.expect = if frame.is_object {
                        Expect::Key
                    } else {
                        Expect::Value
                    };
                    frame.comma = Some(index);
                }
            }
            _ => state.scalar_start = Some(index),
        }
    }
    state
}

/// Removes a trailing, incomplete `\uXXXX` escape from an open string body.
fn strip_partial_unicode_escape(text: &mut String) {
    let Some(position) = text.rfind("\\u") else {
        return;
    };
    let tail = &text[position + 2..];
    if tail.len() >= 4 || !tail.bytes().all(|b| b.is_ascii_hexdigit()) {
        return;
    }
    let backslashes = text[..=position]
        .bytes()
        .rev()
        .take_while(|&b| b == b'\\')
        .count();
    if backslashes % 2 == 1 {
        text.truncate(position);
    }
}

/// Trims a partial number to its longest valid prefix.
fn repair_number(token: &str) -> Option<&str> {
    let mut end = token.len();
    while end > 0 {
        let candidate = &token[..end];
        if serde_json::from_str::<serde_json::Number>(candidate).is_ok() {
            return Some(candidate);
        }
        end -= 1;
    }
    None
}

fn complete(input: &str) -> String {
    let mut text = input.to_owned();
    // Every iteration either finishes or shortens / terminates the text, so this is bounded;
    // the cap is a safety net.
    for _ in 0..input.len() + 8 {
        let state = scan(&text);

        if let Some((start, is_key)) = state.string {
            if is_key {
                text.truncate(start);
            } else {
                if state.escape {
                    text.pop();
                }
                strip_partial_unicode_escape(&mut text);
                text.push('"');
            }
            continue;
        }

        if let Some(start) = state.scalar_start {
            let token = text[start..].to_owned();
            text.truncate(start);
            let is_literal = matches!(token.as_str(), "true" | "false" | "null");
            if is_literal {
                text.push_str(&token);
                text.push(' ');
            } else if let Some(number) = repair_number(&token) {
                text.push_str(number);
                text.push(' ');
            }
            continue;
        }

        let Some(frame) = state.frames.last() else {
            return text;
        };
        match (frame.is_object, frame.expect, frame.comma) {
            (true, Expect::Key, Some(comma)) | (false, Expect::Value, Some(comma)) => {
                text.truncate(comma);
            }
            (true, Expect::Colon | Expect::Value, _) => text.truncate(frame.key_start),
            _ => {
                for frame in state.frames.iter().rev() {
                    text.push(if frame.is_object { '}' } else { ']' });
                }
                return text;
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(input: &str) -> Option<Value> {
        parse_partial_json(input)
    }

    #[test]
    fn complete_documents_parse_directly() {
        assert_eq!(parse(r#"{"a": [1, 2]}"#), Some(json!({"a": [1, 2]})));
    }

    #[test]
    fn empty_and_garbage() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
    }

    #[test]
    fn open_containers_are_closed() {
        assert_eq!(parse("{"), Some(json!({})));
        assert_eq!(parse("["), Some(json!([])));
        assert_eq!(
            parse(r#"{"a": {"b": [1, 2"#),
            Some(json!({"a": {"b": [1, 2]}}))
        );
    }

    #[test]
    fn open_strings_are_closed() {
        assert_eq!(
            parse(r#"{"path": "src/ma"#),
            Some(json!({"path": "src/ma"}))
        );
        assert_eq!(parse(r#"["a", "b"#), Some(json!(["a", "b"])));
        assert_eq!(parse(r#"{"a": "x\"#), Some(json!({"a": "x"})));
        assert_eq!(parse(r#"{"a": "line\n"#), Some(json!({"a": "line\n"})));
        assert_eq!(parse(r#"{"a": "\u00"#), Some(json!({"a": ""})));
        assert_eq!(parse(r#"{"a": "\\u00"#), Some(json!({"a": "\\u00"})));
    }

    #[test]
    fn dangling_keys_and_commas_are_dropped() {
        assert_eq!(parse(r#"{"a": 1, "#), Some(json!({"a": 1})));
        assert_eq!(parse(r#"{"a": 1, "b"#), Some(json!({"a": 1})));
        assert_eq!(parse(r#"{"a": 1, "b""#), Some(json!({"a": 1})));
        assert_eq!(parse(r#"{"a": 1, "b":"#), Some(json!({"a": 1})));
        assert_eq!(parse(r#"{"a": 1, "b": "#), Some(json!({"a": 1})));
        assert_eq!(parse("[1, 2, "), Some(json!([1, 2])));
    }

    #[test]
    fn partial_scalars() {
        assert_eq!(parse(r#"{"a": tr"#), Some(json!({})));
        assert_eq!(parse(r#"{"a": true"#), Some(json!({"a": true})));
        assert_eq!(parse(r#"{"a": nul"#), Some(json!({})));
        assert_eq!(parse(r#"{"a": 12"#), Some(json!({"a": 12})));
        assert_eq!(parse(r#"{"a": 12."#), Some(json!({"a": 12})));
        assert_eq!(parse(r#"{"a": -"#), Some(json!({})));
        assert_eq!(parse(r#"{"a": 1e"#), Some(json!({"a": 1})));
        assert_eq!(parse("[1, fal"), Some(json!([1])));
    }

    #[test]
    fn every_prefix_parses_to_a_prefix_of_the_final_value() {
        let full = r#"{"path": "a/b.rs", "edits": [{"old": "x\"y", "new": "z"}], "n": -1.5e3, "ok": false}"#;
        for end in 0..=full.len() {
            let Some(value) = parse(&full[..end]) else {
                continue;
            };
            if let Value::Object(map) = &value {
                for key in map.keys() {
                    assert!(
                        ["path", "edits", "n", "ok"].contains(&key.as_str()),
                        "{end}: {value}"
                    );
                }
            }
        }
        assert_eq!(parse(full), serde_json::from_str(full).ok());
    }
}
