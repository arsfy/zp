//! Context compaction (after pi): older messages are replaced by a structured summary so a
//! long conversation keeps fitting in the model's context window.
//!
//! [`prepare`] picks the cut point along the session branch (keeping roughly the last
//! `keep_recent_tokens` verbatim, never separating a tool result from its call),
//! [`summarize`] asks the model for the summary (updating the previous one), and the
//! caller appends a compaction entry.

use std::collections::BTreeSet;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zp_agent::AgentMessage;
use zp_ai::{
    AssistantContent, Context, Message, Model, Models, StopReason, StreamOptions, ThinkingLevel,
    UserContent,
};
use zp_session::{EntryData, SessionEntry};

/// When and how much to compact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    /// Compact automatically when the context gets close to the window.
    pub enabled: bool,
    /// Compact once fewer than this many tokens of the window are left.
    pub reserve_tokens: u64,
    /// Recent tokens kept verbatim.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

/// Characters an image counts as when estimating tokens.
const IMAGE_CHARS: usize = 4_800;
/// Tool results are cut to this many characters for the summarizer.
const TOOL_RESULT_MAX_CHARS: usize = 2_000;

const SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY_FORMAT: &str = "## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_RULES: &str = "Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it";

const TURN_PREFIX_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages.";

fn text_chars(content: &[UserContent]) -> usize {
    content
        .iter()
        .map(|block| match block {
            UserContent::Text(text) => text.text.len(),
            UserContent::Image(_) => IMAGE_CHARS,
        })
        .sum()
}

/// A rough token count of `message` (characters / 4).
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars = match message {
        AgentMessage::Llm(Message::User(user)) => text_chars(&user.content),
        AgentMessage::Llm(Message::ToolResult(result)) => text_chars(&result.content),
        AgentMessage::Llm(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => text.text.len(),
                AssistantContent::Thinking(thinking) => thinking.thinking.len(),
                AssistantContent::ToolCall(call) => {
                    call.name.len() + call.arguments.to_string().len()
                }
            })
            .sum(),
        AgentMessage::Custom(custom) => text_chars(&custom.content),
    };
    chars.div_ceil(4) as u64
}

/// The current context size: the last successful response's reported usage plus an
/// estimate of what came after it (everything estimated without one).
pub fn context_tokens(messages: &[AgentMessage]) -> u64 {
    let last = messages.iter().rposition(|message| {
        message.as_assistant().is_some_and(|a| {
            a.usage.total_tokens > 0
                && !matches!(a.stop_reason, StopReason::Error | StopReason::Aborted)
        })
    });
    match last {
        Some(index) => {
            let reported = messages[index]
                .as_assistant()
                .map_or(0, |a| a.usage.total_tokens);
            reported
                + messages[index + 1..]
                    .iter()
                    .map(estimate_tokens)
                    .sum::<u64>()
        }
        None => messages.iter().map(estimate_tokens).sum(),
    }
}

/// Whether a context of `tokens` should be compacted for a window of `context_window`.
pub fn should_compact(tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool {
    settings.enabled
        && context_window > 0
        && tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// Whether a failed response means the request did not fit the context window.
pub fn is_context_overflow(error: &str) -> bool {
    let error = error.to_lowercase();
    [
        "context length",
        "context window",
        "context_length",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "exceeds the context",
        "input is too long",
        "request too large",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

/// What a compaction will summarize.
#[derive(Clone, Debug, PartialEq)]
pub struct Preparation {
    /// The first entry kept verbatim.
    pub first_kept_entry_id: String,
    /// Earlier messages to fold into the summary.
    pub messages_to_summarize: Vec<AgentMessage>,
    /// The start of a turn too large to keep whole (summarized separately).
    pub turn_prefix: Vec<AgentMessage>,
    pub previous_summary: Option<String>,
    /// Files read and changed, carried over from earlier compactions.
    pub read_files: BTreeSet<String>,
    pub modified_files: BTreeSet<String>,
}

/// The outcome of [`summarize`], ready for a compaction entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Compaction {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub details: Value,
}

fn is_user(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Llm(Message::User(_)))
}

fn previous_files(details: Option<&Value>, key: &str) -> BTreeSet<String> {
    details
        .and_then(|d| d.get(key))
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Chooses what to summarize on the branch `path` (root first). `None` when there is
/// nothing worth compacting.
pub fn prepare(path: &[&SessionEntry], settings: &CompactionSettings) -> Option<Preparation> {
    if path
        .last()
        .is_some_and(|e| matches!(e.data, EntryData::Compaction { .. }))
    {
        return None;
    }
    let previous = path
        .iter()
        .rposition(|e| matches!(e.data, EntryData::Compaction { .. }));
    let (start, previous_summary, mut read_files, mut modified_files) = match previous {
        Some(at) => {
            let EntryData::Compaction {
                summary,
                first_kept_entry_id,
                details,
                ..
            } = &path[at].data
            else {
                unreachable!()
            };
            let kept = path[..at].iter().position(|e| &e.id == first_kept_entry_id);
            (
                kept.unwrap_or(at + 1),
                Some(summary.clone()),
                previous_files(details.as_ref(), "readFiles"),
                previous_files(details.as_ref(), "modifiedFiles"),
            )
        }
        None => (0, None, BTreeSet::new(), BTreeSet::new()),
    };

    // Message entries in range (earlier compaction entries contribute nothing).
    let entries: Vec<(&SessionEntry, &AgentMessage)> = path[start..]
        .iter()
        .filter(|e| !matches!(e.data, EntryData::Compaction { .. }))
        .filter_map(|e| Some((*e, e.message()?)))
        .collect();
    let cut_points: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, (_, m))| m.as_tool_result().is_none())
        .map(|(i, _)| i)
        .collect();
    let mut cut = None;
    let mut total = 0;
    for i in (0..entries.len()).rev() {
        total += estimate_tokens(entries[i].1);
        if total >= settings.keep_recent_tokens {
            cut = cut_points
                .iter()
                .copied()
                .find(|&c| c >= i)
                .or(cut_points.last().copied());
            break;
        }
    }
    // A short conversation: keep the last turn and summarize the rest.
    let cut = match cut {
        Some(cut) => cut,
        None => entries.iter().rposition(|(_, m)| is_user(m))?,
    };
    if cut == 0 && previous_summary.is_none() {
        return None;
    }

    let messages = |range: std::ops::Range<usize>| -> Vec<AgentMessage> {
        entries[range].iter().map(|(_, m)| (*m).clone()).collect()
    };
    let (messages_to_summarize, turn_prefix) = if is_user(entries[cut].1) {
        (messages(0..cut), Vec::new())
    } else {
        match entries[..cut].iter().rposition(|(_, m)| is_user(m)) {
            Some(turn_start) => (messages(0..turn_start), messages(turn_start..cut)),
            None => (messages(0..cut), Vec::new()),
        }
    };
    if messages_to_summarize.is_empty() && turn_prefix.is_empty() {
        return None;
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix) {
        collect_file_ops(message, &mut read_files, &mut modified_files);
    }
    Some(Preparation {
        first_kept_entry_id: entries[cut].0.id.clone(),
        messages_to_summarize,
        turn_prefix,
        previous_summary,
        read_files,
        modified_files,
    })
}

fn collect_file_ops(
    message: &AgentMessage,
    read: &mut BTreeSet<String>,
    modified: &mut BTreeSet<String>,
) {
    let Some(assistant) = message.as_assistant() else {
        return;
    };
    for call in assistant.tool_calls() {
        let Some(path) = call.arguments["path"].as_str() else {
            continue;
        };
        match call.name.as_str() {
            "read" => {
                read.insert(path.to_owned());
            }
            "write" | "edit" => {
                modified.insert(path.to_owned());
            }
            _ => {}
        }
    }
}

fn cut_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        &text[..end],
        text.len() - end
    )
}

fn joined(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(UserContent::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The conversation as plain text, so the summarizer does not continue it.
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            AgentMessage::Llm(Message::User(user)) => {
                let text = joined(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            AgentMessage::Custom(custom) => {
                let text = joined(&custom.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                let text = assistant.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls: Vec<String> = assistant
                    .tool_calls()
                    .map(|call| {
                        let args = call
                            .arguments
                            .as_object()
                            .map(|object| {
                                object
                                    .iter()
                                    .map(|(k, v)| format!("{k}={v}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        format!("{}({args})", call.name)
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            AgentMessage::Llm(Message::ToolResult(result)) => {
                let text = joined(&result.content);
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        cut_text(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

async fn ask(models: &Models, model: &Model, prompt: String) -> anyhow::Result<String> {
    let context = Context {
        system_prompt: Some(SYSTEM_PROMPT.to_owned()),
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
    };
    let options = StreamOptions {
        thinking: model.clamp_thinking_level(ThinkingLevel::Off),
        max_tokens: Some(model.max_tokens.clamp(1_024, 32_000)),
        ..StreamOptions::default()
    };
    let reply = models.complete(model, &context, options).await;
    match reply.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            bail!(
                "summarization failed: {}",
                reply.error_message.unwrap_or_default()
            )
        }
        StopReason::Length => {
            bail!("summarization hit the output limit; the summary is incomplete")
        }
        _ => {}
    }
    let text = reply.text();
    if text.trim().is_empty() {
        bail!("the model returned an empty summary");
    }
    Ok(text.trim().to_owned())
}

/// Summarizes `preparation` with `model`; `instructions` focus the summary.
pub async fn summarize(
    models: &Models,
    model: &Model,
    preparation: Preparation,
    instructions: Option<&str>,
) -> anyhow::Result<Compaction> {
    let focus = instructions
        .filter(|i| !i.trim().is_empty())
        .map(|i| format!("\n\nAdditional focus: {}", i.trim()))
        .unwrap_or_default();
    let history = |messages: &[AgentMessage]| {
        let mut prompt = format!(
            "<conversation>\n{}\n</conversation>\n\n",
            serialize_conversation(messages)
        );
        match &preparation.previous_summary {
            Some(previous) => {
                prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n"));
                prompt.push_str("The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\n");
                prompt.push_str(UPDATE_RULES);
                prompt.push_str("\n\nUse this EXACT format:\n\n");
            }
            None => prompt.push_str("The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n"),
        }
        prompt.push_str(SUMMARY_FORMAT);
        prompt.push_str(&focus);
        prompt
    };

    let mut summary = if preparation.messages_to_summarize.is_empty() {
        preparation
            .previous_summary
            .clone()
            .unwrap_or_else(|| "No prior history.".to_owned())
    } else {
        ask(models, model, history(&preparation.messages_to_summarize))
            .await
            .context("could not summarize the conversation")?
    };
    if !preparation.turn_prefix.is_empty() {
        let prompt = format!(
            "# Conversation\n{}\n\n# Instructions\n{TURN_PREFIX_PROMPT}{focus}",
            serialize_conversation(&preparation.turn_prefix)
        );
        let prefix = ask(models, model, prompt)
            .await
            .context("could not summarize the current turn")?;
        summary = format!("{summary}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}");
    }

    let read: Vec<&String> = preparation
        .read_files
        .iter()
        .filter(|f| !preparation.modified_files.contains(*f))
        .collect();
    let modified: Vec<&String> = preparation.modified_files.iter().collect();
    let mut sections = Vec::new();
    if !read.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !modified.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !sections.is_empty() {
        summary = format!("{summary}\n\n{}", sections.join("\n\n"));
    }
    Ok(Compaction {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id,
        details: json!({"readFiles": read, "modifiedFiles": modified}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use zp_ai::{AssistantMessage, TextContent, ToolCall, ToolResultMessage};
    use zp_session::Session;

    fn assistant(text: &str, calls: &[(&str, &str)]) -> AgentMessage {
        let mut message = AssistantMessage::empty("faux", "faux", "faux-1");
        message
            .content
            .push(AssistantContent::Text(TextContent { text: text.into() }));
        for (i, (name, path)) in calls.iter().enumerate() {
            message.content.push(AssistantContent::ToolCall(ToolCall {
                id: format!("c{i}-{text}"),
                name: (*name).into(),
                arguments: json!({"path": path}),
            }));
        }
        message.into()
    }

    fn result(text: &str) -> AgentMessage {
        let call = ToolCall {
            id: "x".into(),
            name: "read".into(),
            arguments: json!({}),
        };
        AgentMessage::Llm(Message::ToolResult(ToolResultMessage::text(
            &call, text, false,
        )))
    }

    fn settings(keep: u64) -> CompactionSettings {
        CompactionSettings {
            keep_recent_tokens: keep,
            ..CompactionSettings::default()
        }
    }

    #[test]
    fn estimates_and_thresholds() {
        assert_eq!(estimate_tokens(&AgentMessage::user("abcdefgh")), 2);
        let mut answered = AssistantMessage::empty("faux", "faux", "faux-1");
        answered.usage.total_tokens = 1000;
        let messages = vec![
            AgentMessage::user("x".repeat(400)),
            answered.into(),
            AgentMessage::user("y".repeat(40)),
        ];
        assert_eq!(context_tokens(&messages), 1010);
        let s = CompactionSettings::default();
        assert!(!should_compact(100_000, 128_000, &s));
        assert!(should_compact(120_000, 128_000, &s));
        assert!(!should_compact(120_000, 0, &s));
        assert!(is_context_overflow(
            "400: prompt is too long: 210000 tokens > 200000 maximum"
        ));
        assert!(!is_context_overflow("rate limited"));
    }

    #[test]
    fn cut_points_and_repeated_compaction() {
        let mut session = Session::in_memory(Path::new("/p"));
        let big = "z".repeat(400); // 100 tokens
        session
            .append_message(AgentMessage::user("first task"))
            .unwrap();
        session
            .append_message(assistant(&big, &[("read", "a.rs")]))
            .unwrap();
        session.append_message(result(&big)).unwrap();
        session
            .append_message(assistant("done one", &[("edit", "a.rs")]))
            .unwrap();
        let second = session
            .append_message(AgentMessage::user("second task"))
            .unwrap();
        session
            .append_message(assistant(&big, &[("read", "b.rs")]))
            .unwrap();
        session.append_message(result(&big)).unwrap();
        session.append_message(assistant("done two", &[])).unwrap();

        // Keeping ~150 tokens cuts inside the second turn at an assistant message.
        let prep = prepare(&session.branch_path(None), &settings(150)).unwrap();
        assert_ne!(prep.first_kept_entry_id, second);
        assert_eq!(prep.messages_to_summarize.len(), 4);
        assert_eq!(prep.turn_prefix.len(), 1, "the second turn up to the cut");
        assert!(prep.read_files.contains("a.rs"));
        assert!(prep.modified_files.contains("a.rs"));

        // A large budget keeps the last turn whole.
        let prep = prepare(&session.branch_path(None), &settings(1_000_000)).unwrap();
        assert_eq!(prep.first_kept_entry_id, second);
        assert!(prep.turn_prefix.is_empty());

        // After a compaction, the kept messages are summarized next time, with the old
        // summary and file lists.
        session
            .append_compaction(
                "old summary".into(),
                &second,
                5000,
                Some(json!({"readFiles": ["z.rs"], "modifiedFiles": ["a.rs"]})),
            )
            .unwrap();
        assert!(
            prepare(&session.branch_path(None), &settings(150)).is_none(),
            "just compacted"
        );
        session
            .append_message(AgentMessage::user("third task"))
            .unwrap();
        session
            .append_message(assistant("done three", &[]))
            .unwrap();
        let prep = prepare(&session.branch_path(None), &settings(1_000_000)).unwrap();
        assert_eq!(prep.previous_summary.as_deref(), Some("old summary"));
        assert_eq!(
            prep.messages_to_summarize.len(),
            4,
            "the second turn, kept last time"
        );
        assert!(prep.read_files.contains("z.rs"));
    }

    #[test]
    fn serializes_for_the_summarizer() {
        let text = serialize_conversation(&[
            AgentMessage::user("fix it"),
            assistant("looking", &[("read", "src/a.rs")]),
            result(&"r".repeat(3000)),
        ]);
        assert!(text.starts_with("[User]: fix it\n\n[Assistant]: looking\n\n[Assistant tool calls]: read(path=\"src/a.rs\")"));
        assert!(text.contains("[... 1000 more characters truncated]"));
    }

    #[tokio::test]
    async fn summarizes_with_file_lists() {
        let faux = zp_ai::FauxProvider::default();
        faux.push(zp_ai::FauxResponse::text("## Goal\nShip it"));
        let mut models = Models::new();
        models.set_provider(std::sync::Arc::new(faux.clone()));
        let prep = Preparation {
            first_kept_entry_id: "k".into(),
            messages_to_summarize: vec![AgentMessage::user("hello")],
            turn_prefix: Vec::new(),
            previous_summary: Some("before".into()),
            read_files: ["a.rs".to_owned(), "b.rs".to_owned()].into(),
            modified_files: ["b.rs".to_owned()].into(),
        };
        let result = summarize(&models, &faux.model(), prep, Some("the API"))
            .await
            .unwrap();
        assert_eq!(
            result.summary,
            "## Goal\nShip it\n\n<read-files>\na.rs\n</read-files>\n\n<modified-files>\nb.rs\n</modified-files>"
        );
        assert_eq!(result.details["modifiedFiles"], json!(["b.rs"]));
        let request = &faux.requests()[0];
        let Message::User(user) = &request.messages[0] else {
            panic!()
        };
        let prompt = joined(&user.content);
        assert!(prompt.contains("<previous-summary>\nbefore"));
        assert!(prompt.ends_with("Additional focus: the API"));
    }
}
