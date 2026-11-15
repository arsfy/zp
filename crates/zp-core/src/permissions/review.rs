//! Auto-review: a model decides whether a tool call is safe to run without the user.
//! It can only approve or hand the call to the user, never deny it on its own.

use std::path::Path;

use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;
use zp_ai::{Context, Message, Model, Models, StopReason, StreamOptions, ThinkingLevel};

const REVIEW_PROMPT: &str = "You review tool calls of an AI coding agent before they run, \
deciding whether a call may run without asking the user.

Answer allow only if the call is clearly safe and fits the user's request. Answer ask if it \
could delete or overwrite data that version control cannot restore, reads or sends secrets or \
credentials, sends project data over the network, installs software outside the project, \
changes system settings, affects other machines (deploys, pushes, publishes), or if you are \
unsure.

Reply with only a JSON object: {\"decision\": \"allow\" or \"ask\", \"reason\": \"one short sentence\"}";

/// Longest excerpts sent for review.
const INTENT_CHARS: usize = 1500;
const ARGS_CHARS: usize = 4000;

/// The review model's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Safe to run (with the model's reason).
    Allow(String),
    /// The user should decide (with the model's reason).
    Ask(String),
}

#[derive(Deserialize)]
struct Answer {
    decision: String,
    #[serde(default)]
    reason: String,
}

/// Reads the model's reply: a JSON object, possibly wrapped in prose or a code fence.
pub fn parse_verdict(reply: &str) -> Verdict {
    let json = reply
        .find('{')
        .zip(reply.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<Answer>(&reply[start..=end]).ok());
    match json {
        Some(answer) if answer.decision.trim().eq_ignore_ascii_case("allow") => {
            Verdict::Allow(answer.reason)
        }
        Some(answer) if !answer.reason.trim().is_empty() => Verdict::Ask(answer.reason),
        Some(_) => Verdict::Ask("not clearly safe".into()),
        None => Verdict::Ask("the review model gave no clear answer".into()),
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str(" …");
    out
}

/// Asks `model` to review a call of `tool` with `args` in project `cwd`, for a user who
/// asked for `intent`.
pub async fn review_tool_call(
    models: &Models,
    model: &Model,
    cwd: &Path,
    intent: Option<&str>,
    tool: &str,
    args: &Value,
) -> anyhow::Result<Verdict> {
    let call = match (tool, args["command"].as_str()) {
        ("bash", Some(command)) => format!("Shell command:\n{}", excerpt(command, ARGS_CHARS)),
        _ => format!(
            "Tool `{tool}` with arguments:\n{}",
            excerpt(&args.to_string(), ARGS_CHARS)
        ),
    };
    let request = format!(
        "Project directory: {}\n\nThe user's request:\n{}\n\n{call}",
        cwd.display(),
        intent.map_or_else(|| "(unknown)".to_owned(), |i| excerpt(i, INTENT_CHARS)),
    );
    let context = Context {
        system_prompt: Some(REVIEW_PROMPT.to_owned()),
        messages: vec![Message::user(request)],
        tools: Vec::new(),
    };
    let options = StreamOptions {
        thinking: model.clamp_thinking_level(ThinkingLevel::Off),
        max_tokens: Some(300),
        ..StreamOptions::default()
    };
    let reply = models.complete(model, &context, options).await;
    if matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted) {
        bail!("{}", reply.error_message.unwrap_or_default());
    }
    Ok(parse_verdict(&reply.text()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_answers() {
        assert_eq!(
            parse_verdict(r#"{"decision": "allow", "reason": "runs the tests"}"#),
            Verdict::Allow("runs the tests".into())
        );
        assert_eq!(
            parse_verdict("```json\n{\"decision\":\"ASK\",\"reason\":\"uploads files\"}\n```"),
            Verdict::Ask("uploads files".into())
        );
        assert!(matches!(parse_verdict("Sure, allow it."), Verdict::Ask(_)));
        assert!(matches!(
            parse_verdict(r#"{"decision": "maybe"}"#),
            Verdict::Ask(_)
        ));
    }
}
