//! One-line summaries of tool calls for UIs.

use serde_json::Value;

/// A short summary of a call's arguments: the command for `bash`, the path for file
/// tools, the pattern for searches, else compact JSON. At most `max_chars` characters.
pub fn describe_call(tool: &str, args: &Value, max_chars: usize) -> String {
    let field = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or_default();
    let summary = match tool {
        "bash" => format!("$ {}", field("command")),
        "read" => {
            let mut text = field("path").to_owned();
            if let Some(offset) = args.get("offset").and_then(Value::as_i64) {
                text.push_str(&format!(":{offset}"));
            }
            text
        }
        "write" | "edit" | "ls" => field("path").to_owned(),
        "grep" | "find" => {
            let path = field("path");
            if path.is_empty() {
                field("pattern").to_owned()
            } else {
                format!("{} in {path}", field("pattern"))
            }
        }
        "todo" => {
            let todos = args.get("todos").and_then(Value::as_array);
            let count = todos.map_or(0, Vec::len);
            let current = todos
                .and_then(|todos| todos.iter().find(|item| item["status"] == "in_progress"))
                .and_then(|item| item["content"].as_str());
            match current {
                Some(current) => format!("{count} tasks · {current}"),
                None => format!("{count} tasks"),
            }
        }
        "ask" => args
            .get("questions")
            .and_then(Value::as_array)
            .and_then(|questions| questions.first())
            .and_then(|question| question["question"].as_str())
            .unwrap_or_default()
            .to_owned(),
        "subagent" => field("task").to_owned(),
        _ => args.to_string(),
    };
    shorten(&summary, max_chars)
}

/// First line of `text`, cut to `max_chars` characters with `…` when shortened.
pub fn shorten(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    let multiline = text.lines().nth(1).is_some();
    match line.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None if multiline => format!("{line}…"),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summaries() {
        assert_eq!(
            describe_call("bash", &json!({"command": "cargo test\necho"}), 80),
            "$ cargo test…"
        );
        assert_eq!(
            describe_call("read", &json!({"path": "a.rs", "offset": 10}), 80),
            "a.rs:10"
        );
        assert_eq!(
            describe_call("grep", &json!({"pattern": "fn", "path": "src"}), 80),
            "fn in src"
        );
        assert_eq!(describe_call("edit", &json!({"path": "x"}), 80), "x");
        assert_eq!(describe_call("other", &json!({"a": 1}), 80), "{\"a\":1}");
        assert_eq!(
            describe_call("bash", &json!({"command": "abcdef"}), 4),
            "$ ab…"
        );
    }
}
