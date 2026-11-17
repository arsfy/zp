//! Auto-review: a model decides whether a tool call is safe to run without the user.
//! It can only approve or hand the call to the user, never deny it on its own.

use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zp_agent::{AgentTool as _, ToolContext};
use zp_ai::{
    Context, Message, Model, Models, StopReason, StreamOptions, ThinkingLevel, Tool, ToolCall,
    ToolResultMessage, UserContent,
};
use zp_tools::ReadTool;

use super::{Grants, PermissionPolicy};

const REVIEW_PROMPT: &str = "You review tool calls of an AI coding agent before they run, \
deciding whether a call may run without asking the user.

Answer allow only if the call is clearly safe and fits the user's request. Answer ask if it \
could delete or overwrite data that version control cannot restore, reads or sends secrets or \
credentials, sends project data over the network, installs software outside the project, \
changes system settings, affects other machines (deploys, pushes, publishes), or if you are \
unsure.

When the call runs something whose effect you cannot tell from the call alone (a script, \
a Makefile or package.json target, a config file), use the read tool to look at it first. \
Only files in the project can be read. Read only what you need.

Reply with only a JSON object: {\"decision\": \"allow\" or \"ask\", \"reason\": \"one short sentence\"}";

/// Longest excerpts sent for review.
const INTENT_CHARS: usize = 1500;
const ARGS_CHARS: usize = 4000;
/// Longest file excerpt the review model sees.
const READ_CHARS: usize = 20_000;
/// Most files read in one review; a model still reading after that gets the user.
const MAX_READS: usize = 6;

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

/// The text of a tool result: text parts, with images left out.
fn result_text(content: &[UserContent]) -> String {
    content
        .iter()
        .map(|part| part.as_text().unwrap_or("(image not shown)"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Runs a read requested by the review model; only files the agent itself may read
/// without asking (the project, allowed directories) are available.
async fn run_read(
    reader: &ReadTool,
    policy: &PermissionPolicy,
    grants: &Grants,
    call: &ToolCall,
) -> (String, bool) {
    if call.name != reader.name() {
        return (
            format!("Unknown tool `{}`; only `read` is available.", call.name),
            true,
        );
    }
    let paths = super::tool_paths(&call.name, &call.arguments, &policy.root);
    if paths.is_empty() {
        return ("`path` is required.".to_owned(), true);
    }
    if !paths.iter().all(|path| policy.in_scope(path, grants)) {
        return (
            "Only files in the project or its allowed directories can be read.".to_owned(),
            true,
        );
    }
    let ctx = ToolContext::detached(call.id.clone(), CancellationToken::new());
    match reader.execute(call.arguments.clone(), ctx).await {
        Ok(result) => (excerpt(&result_text(&result.content), READ_CHARS), false),
        Err(error) => (format!("{error:#}"), true),
    }
}

/// Asks `model` to review a call of `tool` with `args` under `policy`, for a user who
/// asked for `intent`. The model may read files in scope before it decides.
pub async fn review_tool_call(
    models: &Models,
    model: &Model,
    policy: &PermissionPolicy,
    grants: &Grants,
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
        policy.root.display(),
        intent.map_or_else(|| "(unknown)".to_owned(), |i| excerpt(i, INTENT_CHARS)),
    );
    let reader = ReadTool::new(&policy.root);
    let mut context = Context {
        system_prompt: Some(REVIEW_PROMPT.to_owned()),
        messages: vec![Message::user(request)],
        tools: vec![Tool {
            name: reader.name().to_owned(),
            description: "Read a text file of the project. Output is truncated; use \
                          offset/limit (1-indexed lines) for long files."
                .to_owned(),
            parameters: reader.parameters(),
        }],
    };
    let options = StreamOptions {
        thinking: model.clamp_thinking_level(ThinkingLevel::Off),
        max_tokens: Some(600),
        ..StreamOptions::default()
    };
    let mut reads = 0;
    loop {
        let reply = models.complete(model, &context, options.clone()).await;
        if matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted) {
            bail!("{}", reply.error_message.unwrap_or_default());
        }
        let calls: Vec<ToolCall> = reply.tool_calls().cloned().collect();
        if calls.is_empty() {
            return Ok(parse_verdict(&reply.text()));
        }
        if reads >= MAX_READS {
            return Ok(Verdict::Ask(
                "the review model kept reading files without deciding".into(),
            ));
        }
        context.messages.push(Message::Assistant(reply));
        for call in &calls {
            reads += 1;
            let (text, is_error) = if reads > MAX_READS {
                ("Read limit reached; decide now.".to_owned(), true)
            } else {
                run_read(&reader, policy, grants, call).await
            };
            context
                .messages
                .push(Message::ToolResult(ToolResultMessage::text(
                    call, text, is_error,
                )));
        }
    }
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
