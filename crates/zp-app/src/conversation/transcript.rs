//! The displayed transcript: rows built from agent messages and kept current from agent
//! events, independent of rendering.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;
use zp_agent::{AgentEvent, AgentMessage};
use zp_ai::{
    AssistantMessage, AssistantMessageEvent, Message, StopReason, StreamAccumulator, UserContent,
    now_ms,
};
use zp_core::UsageTotals;
use zp_session::{EntryData, Session, SessionEntry};

/// Execution state of a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

/// What is known about a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolState {
    pub status: ToolStatus,
    /// Latest (partial or final) text output.
    pub output: String,
    /// Structured result details (e.g. a file change).
    pub details: Option<Value>,
}

/// When a message was recorded, with the active run time for an assistant response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageTiming {
    pub timestamp: i64,
    pub elapsed_seconds: Option<u64>,
}

impl MessageTiming {
    pub(super) fn from_entry(entry: &SessionEntry) -> Option<Self> {
        let EntryData::Message {
            elapsed_seconds, ..
        } = &entry.data
        else {
            return None;
        };
        Some(Self {
            timestamp: entry.timestamp,
            elapsed_seconds: *elapsed_seconds,
        })
    }
}

/// One rendered row of the transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// A user message.
    User {
        text: String,
        images: usize,
        /// Files attached with `@path`.
        files: Vec<String>,
        sent_at: i64,
        ordinal: usize,
    },
    /// A model response with its tool calls. While streaming, the current content is
    /// read through [`Transcript::assistant`].
    Assistant {
        /// Boxed: a response is several times larger than the other rows.
        message: Box<AssistantMessage>,
        completion: Option<MessageTiming>,
        ordinal: usize,
    },
    /// A custom message meant to be shown.
    Notice {
        kind: String,
        text: String,
        ordinal: usize,
    },
}

impl Row {
    /// Index of the row's message in the agent transcript (and so in the session
    /// context), used to find its session entry.
    pub fn ordinal(&self) -> usize {
        match self {
            Self::User { ordinal, .. }
            | Self::Assistant { ordinal, .. }
            | Self::Notice { ordinal, .. } => *ordinal,
        }
    }
}

/// What a batch of events changed, for list remeasurement and status updates.
#[derive(Debug, Default)]
pub struct Changes {
    /// Rows whose content changed.
    pub dirty: BTreeSet<usize>,
    /// Usage totals changed.
    pub usage: bool,
    /// The run ended.
    pub finished: bool,
}

/// Rows plus tool states and usage.
#[derive(Default)]
pub struct Transcript {
    rows: Vec<Row>,
    tools: HashMap<String, ToolState>,
    tool_rows: HashMap<String, usize>,
    streaming: Option<(usize, StreamAccumulator)>,
    active_thinking: HashSet<usize>,
    usage: UsageTotals,
    /// Ordinal of the next finished message.
    next_ordinal: usize,
    /// Used to estimate elapsed time for responses saved before the run clock was recorded.
    last_user_at: Option<i64>,
}

fn text_of(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(UserContent::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Transcript {
    /// Restores the transcript and message timing from the active session branch.
    pub(super) fn from_session(session: &Session) -> Self {
        let mut transcript = Self::default();
        let mut changes = Changes::default();
        let context = session.context();
        for (message, id) in context.messages.iter().zip(&context.entry_ids) {
            let timing = session.entry(id).and_then(MessageTiming::from_entry);
            transcript.finish_message(message, timing, &mut changes);
        }
        transcript
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn usage(&self) -> &UsageTotals {
        &self.usage
    }

    /// Usage of the unfinished response, excluded from the completed totals.
    pub fn streaming_usage(&self) -> Option<&zp_ai::Usage> {
        self.streaming
            .as_ref()
            .map(|(_, accumulator)| &accumulator.message().usage)
    }

    /// The tool call `id`.
    /// The row of the assistant message that made tool call `id`.
    pub fn row_of_tool(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|row| match row {
            Row::Assistant { message, .. } => message.tool_calls().any(|call| call.id == id),
            _ => false,
        })
    }

    pub fn tool(&self, id: &str) -> Option<&ToolState> {
        self.tools.get(id)
    }

    /// The ID and result details of the last successful call of `tool`.
    pub fn latest_tool_details(&self, tool: &str) -> Option<(&str, &Value)> {
        self.rows.iter().rev().find_map(|row| match row {
            Row::Assistant { message, .. } => {
                let calls: Vec<_> = message.tool_calls().collect();
                calls.into_iter().rev().find_map(|call| {
                    let state = self.tools.get(&call.id)?;
                    (call.name == tool && state.status == ToolStatus::Done)
                        .then_some(state.details.as_ref())
                        .flatten()
                        .map(|details| (call.id.as_str(), details))
                })
            }
            _ => None,
        })
    }

    /// The assistant message of `row`, including in-flight content.
    pub fn assistant(&self, row: usize) -> Option<&AssistantMessage> {
        if let Some((index, accumulator)) = &self.streaming
            && *index == row
        {
            return Some(accumulator.message());
        }
        match self.rows.get(row)? {
            Row::Assistant { message, .. } => Some(message),
            _ => None,
        }
    }

    /// Whether `row` is the response being streamed.
    /// The row of the response being streamed, if any.
    pub fn streaming_row(&self) -> Option<usize> {
        self.streaming.as_ref().map(|(row, _)| *row)
    }

    pub fn is_streaming(&self, row: usize) -> bool {
        self.streaming
            .as_ref()
            .is_some_and(|(index, _)| *index == row)
    }

    /// Whether this reasoning block is still receiving content.
    pub fn thinking_is_running(&self, row: usize, block: usize) -> bool {
        self.is_streaming(row) && self.active_thinking.contains(&block)
    }

    /// The last row if it is a failed or aborted response.
    pub fn failed_response(&self) -> Option<usize> {
        let last = self.rows.len().checked_sub(1)?;
        let message = self.assistant(last)?;
        matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
            .then_some(last)
            .filter(|_| !self.is_streaming(last))
    }

    fn push(&mut self, row: Row, changes: &mut Changes) -> usize {
        self.rows.push(row);
        let index = self.rows.len() - 1;
        changes.dirty.insert(index);
        index
    }

    fn touch_tool(&mut self, id: &str, changes: &mut Changes) {
        if let Some(&row) = self.tool_rows.get(id) {
            changes.dirty.insert(row);
        }
    }

    /// Records a completed message.
    fn finish_message(
        &mut self,
        message: &AgentMessage,
        timing: Option<MessageTiming>,
        changes: &mut Changes,
    ) {
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        match message {
            AgentMessage::Llm(Message::User(user)) => {
                self.last_user_at = Some(timing.map_or(user.timestamp, |timing| timing.timestamp));
                let images = user
                    .content
                    .iter()
                    .filter(|c| matches!(c, UserContent::Image(_)))
                    .count();
                let full = text_of(&user.content);
                let (files, text) = zp_core::attachments::split_file_blocks(&full);
                self.push(
                    Row::User {
                        text: text.to_owned(),
                        images,
                        files,
                        sent_at: user.timestamp,
                        ordinal,
                    },
                    changes,
                );
            }
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                let mut completion = timing.unwrap_or_else(|| MessageTiming {
                    timestamp: now_ms(),
                    elapsed_seconds: None,
                });
                if completion.elapsed_seconds.is_none() {
                    let started = self
                        .last_user_at
                        .filter(|&sent_at| sent_at <= completion.timestamp)
                        .unwrap_or(assistant.timestamp);
                    completion.elapsed_seconds =
                        u64::try_from(completion.timestamp.saturating_sub(started) / 1000).ok();
                }
                self.active_thinking.clear();
                let finished = Row::Assistant {
                    message: Box::new(assistant.clone()),
                    completion: Some(completion),
                    ordinal,
                };
                let row = match self.streaming.take() {
                    Some((row, _)) => {
                        self.rows[row] = finished;
                        changes.dirty.insert(row);
                        row
                    }
                    None => self.push(finished, changes),
                };
                for call in assistant.tool_calls() {
                    self.tool_rows.insert(call.id.clone(), row);
                }
                self.usage.add(&assistant.usage);
                if !matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    self.usage.last_context_tokens = assistant.usage.total_tokens;
                }
                changes.usage = true;
            }
            AgentMessage::Llm(Message::ToolResult(result)) => {
                if result.tool_name == zp_core::subagents::TOOL_NAME
                    && let Some(report) = result
                        .details
                        .as_ref()
                        .and_then(zp_core::subagents::Report::from_details)
                {
                    self.usage.add_subagent(&report.usage);
                    changes.usage = true;
                }
                let state = ToolState {
                    status: if result.is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Done
                    },
                    output: text_of(&result.content),
                    details: result.details.clone(),
                };
                self.tools.insert(result.tool_call_id.clone(), state);
                self.touch_tool(&result.tool_call_id, changes);
            }
            AgentMessage::Custom(custom) => {
                if custom.display {
                    self.push(
                        Row::Notice {
                            kind: custom.custom_type.clone(),
                            text: zp_core::changes::restore_summary(custom)
                                .unwrap_or_else(|| text_of(&custom.content)),
                            ordinal,
                        },
                        changes,
                    );
                }
            }
        }
    }

    /// Applies one agent event.
    pub(super) fn apply(
        &mut self,
        event: &AgentEvent,
        timing: Option<MessageTiming>,
        changes: &mut Changes,
    ) {
        match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Llm(Message::Assistant(message)),
            } => {
                self.active_thinking.clear();
                let row = self.push(
                    Row::Assistant {
                        message: Box::new(message.clone()),
                        completion: None,
                        ordinal: self.next_ordinal,
                    },
                    changes,
                );
                self.streaming = Some((row, StreamAccumulator::new(message.clone())));
            }
            AgentEvent::MessageUpdate { event } => {
                if let Some((row, accumulator)) = &mut self.streaming {
                    match event {
                        AssistantMessageEvent::ThinkingStart { index, .. } => {
                            self.active_thinking.insert(*index);
                        }
                        AssistantMessageEvent::ThinkingEnd { index, .. } => {
                            self.active_thinking.remove(index);
                        }
                        _ => {}
                    }
                    accumulator.apply(event);
                    changes.dirty.insert(*row);
                }
            }
            AgentEvent::MessageEnd { message } => self.finish_message(message, timing, changes),
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                self.tools.insert(
                    tool_call_id.clone(),
                    ToolState {
                        status: ToolStatus::Running,
                        output: String::new(),
                        details: None,
                    },
                );
                self.touch_tool(tool_call_id, changes);
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(state) = self.tools.get_mut(tool_call_id) {
                    state.output = text_of(&partial_result.content);
                    if partial_result.details.is_some() {
                        state.details.clone_from(&partial_result.details);
                    }
                }
                self.touch_tool(tool_call_id, changes);
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                self.tools.insert(
                    tool_call_id.clone(),
                    ToolState {
                        status: if *is_error {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Done
                        },
                        output: text_of(&result.content),
                        details: result.details.clone(),
                    },
                );
                self.touch_tool(tool_call_id, changes);
            }
            AgentEvent::AgentEnd { .. } => {
                self.active_thinking.clear();
                // A run that ended mid-stream (should not happen) leaves no live row.
                if let Some((row, accumulator)) = self.streaming.take() {
                    let ordinal = self.rows[row].ordinal();
                    self.rows[row] = Row::Assistant {
                        message: Box::new(accumulator.into_message()),
                        completion: None,
                        ordinal,
                    };
                    changes.dirty.insert(row);
                }
                changes.finished = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zp_agent::AgentToolResult;
    use zp_ai::{
        AssistantContent, AssistantMessageEvent, TextContent, ToolCall, ToolResultMessage, Usage,
    };

    fn restored(messages: &[AgentMessage]) -> Transcript {
        let mut session = Session::in_memory(std::path::Path::new("."));
        for message in messages {
            session.append_message(message.clone()).unwrap();
        }
        Transcript::from_session(&session)
    }

    #[test]
    fn restores_legacy_completion_time_and_estimates_elapsed_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.jsonl");
        let session = Session::in_memory(dir.path());
        let mut user = zp_ai::UserMessage::text("hello");
        user.timestamp = 10_000;
        let mut reply = AssistantMessage::empty("faux", "faux", "m");
        reply.timestamp = 20_000;
        let lines = [
            serde_json::to_value(session.header()).unwrap(),
            json!({"type": "message", "id": "a", "parentId": null,
                "timestamp": 12_000, "message": Message::User(user)}),
            json!({"type": "message", "id": "b", "parentId": "a",
                "timestamp": 95_000, "message": Message::Assistant(reply)}),
        ];
        let text = lines.map(|line| line.to_string()).join("\n");
        std::fs::write(&file, text).unwrap();

        let transcript = Transcript::from_session(&Session::open(&file).unwrap());
        assert!(matches!(
            transcript.rows()[0],
            Row::User {
                sent_at: 10_000,
                ..
            }
        ));
        assert!(matches!(
            transcript.rows()[1],
            Row::Assistant {
                completion: Some(MessageTiming {
                    timestamp: 95_000,
                    elapsed_seconds: Some(83),
                }),
                ..
            }
        ));
    }

    fn assistant(blocks: Vec<AssistantContent>, stop: StopReason) -> AssistantMessage {
        let mut message = AssistantMessage::empty("faux", "faux", "m");
        message.content = blocks;
        message.stop_reason = stop;
        message.usage = Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..Usage::default()
        };
        message
    }

    fn call(id: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command": "ls"}),
        })
    }

    #[test]
    fn restores_rows_and_tool_results() {
        let tool_call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({}),
        };
        let messages: Vec<AgentMessage> = vec![
            AgentMessage::user("hi"),
            assistant(vec![call("c1")], StopReason::ToolUse).into(),
            ToolResultMessage::text(&tool_call, "out", false).into(),
            assistant(
                vec![AssistantContent::Text(TextContent {
                    text: "done".into(),
                })],
                StopReason::Stop,
            )
            .into(),
        ];
        let transcript = restored(&messages);
        assert_eq!(transcript.len(), 3, "tool results live in their call's row");
        let ordinals: Vec<usize> = transcript.rows().iter().map(Row::ordinal).collect();
        assert_eq!(ordinals, [0, 1, 3], "the tool result is message 2");
        assert_eq!(transcript.tool("c1").unwrap().output, "out");
        assert_eq!(transcript.tool("c1").unwrap().status, ToolStatus::Done);
        assert_eq!(transcript.usage().output, 10);
        assert_eq!(transcript.failed_response(), None);
    }

    #[test]
    fn thinking_finishes_independently_of_the_response() {
        let skeleton = AssistantMessage::empty("faux", "faux", "m");
        let mut transcript = Transcript::default();
        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::MessageStart {
                message: skeleton.clone().into(),
            },
            None,
            &mut changes,
        );
        transcript.apply(
            &AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::ThinkingStart {
                    index: 0,
                    redacted: false,
                },
            },
            None,
            &mut changes,
        );
        assert!(transcript.thinking_is_running(0, 0));
        assert!(!transcript.thinking_is_running(1, 0));
        assert!(!transcript.thinking_is_running(0, 1));
        changes.dirty.clear();
        transcript.apply(
            &AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::ThinkingEnd {
                    index: 0,
                    signature: None,
                },
            },
            None,
            &mut changes,
        );
        assert!(!transcript.thinking_is_running(0, 0));
        assert!(transcript.is_streaming(0));
        assert!(changes.dirty.contains(&0));

        // Missing ThinkingEnd events must not leave a completed block active.
        for end in [
            AgentEvent::MessageEnd {
                message: skeleton.clone().into(),
            },
            AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
        ] {
            let mut transcript = Transcript::default();
            transcript.apply(
                &AgentEvent::MessageStart {
                    message: skeleton.clone().into(),
                },
                None,
                &mut changes,
            );
            transcript.apply(
                &AgentEvent::MessageUpdate {
                    event: AssistantMessageEvent::ThinkingStart {
                        index: 0,
                        redacted: false,
                    },
                },
                None,
                &mut changes,
            );
            assert!(transcript.thinking_is_running(0, 0));
            transcript.apply(&end, None, &mut changes);
            assert!(!transcript.thinking_is_running(0, 0));
            transcript.apply(
                &AgentEvent::MessageStart {
                    message: skeleton.clone().into(),
                },
                None,
                &mut changes,
            );
            assert!(!transcript.thinking_is_running(1, 0));
        }
    }

    #[test]
    fn streaming_usage_is_replaced_by_final_usage_without_double_counting() {
        let first = assistant(Vec::new(), StopReason::Stop);
        let mut transcript = restored(&[first.clone().into()]);
        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::MessageStart {
                message: AssistantMessage::empty("faux", "faux", "m").into(),
            },
            None,
            &mut changes,
        );
        let usage = Usage {
            input: 10,
            cache_read: 90,
            output: 2,
            total_tokens: 102,
            ..Usage::default()
        };
        transcript.apply(
            &AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::Usage {
                    usage: usage.clone(),
                },
            },
            None,
            &mut changes,
        );
        assert_eq!(transcript.streaming_usage(), Some(&usage));
        assert_eq!(transcript.usage().output, 5);
        let mut final_message = first;
        final_message.usage = Usage {
            output: 20,
            total_tokens: 120,
            ..usage
        };
        transcript.apply(
            &AgentEvent::MessageEnd {
                message: final_message.into(),
            },
            None,
            &mut changes,
        );
        assert!(transcript.streaming_usage().is_none());
        assert_eq!(transcript.usage().output, 25);
        assert_eq!(transcript.usage().cache_read, 90);
    }

    #[test]
    fn streams_and_tracks_tools() {
        let mut transcript = restored(&[AgentMessage::user("go")]);
        let mut changes = Changes::default();
        let skeleton = AssistantMessage::empty("faux", "faux", "m");
        transcript.apply(
            &AgentEvent::MessageStart {
                message: skeleton.clone().into(),
            },
            None,
            &mut changes,
        );
        for event in [
            AssistantMessageEvent::TextStart { index: 0 },
            AssistantMessageEvent::TextDelta {
                index: 0,
                delta: "Hel".into(),
            },
            AssistantMessageEvent::TextDelta {
                index: 0,
                delta: "lo".into(),
            },
        ] {
            transcript.apply(&AgentEvent::MessageUpdate { event }, None, &mut changes);
        }
        assert!(transcript.is_streaming(1));
        assert_eq!(transcript.assistant(1).unwrap().text(), "Hello");
        assert_eq!(changes.dirty.iter().copied().collect::<Vec<_>>(), [1]);

        let final_message = assistant(
            vec![
                AssistantContent::Text(TextContent {
                    text: "Hello".into(),
                }),
                call("c1"),
            ],
            StopReason::ToolUse,
        );
        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::MessageEnd {
                message: final_message.into(),
            },
            None,
            &mut changes,
        );
        assert!(!transcript.is_streaming(1) && changes.usage);

        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::ToolExecutionStart {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                args: json!({}),
            },
            None,
            &mut changes,
        );
        transcript.apply(
            &AgentEvent::ToolExecutionUpdate {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                partial_result: AgentToolResult::text("partial"),
            },
            None,
            &mut changes,
        );
        assert_eq!(transcript.tool("c1").unwrap().status, ToolStatus::Running);
        assert_eq!(transcript.tool("c1").unwrap().output, "partial");
        transcript.apply(
            &AgentEvent::ToolExecutionEnd {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                result: AgentToolResult::error("boom"),
                is_error: true,
                duration_ms: Some(3),
            },
            None,
            &mut changes,
        );
        assert_eq!(transcript.tool("c1").unwrap().status, ToolStatus::Error);
        assert_eq!(changes.dirty.iter().copied().collect::<Vec<_>>(), [1]);

        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::MessageEnd {
                message: assistant(Vec::new(), StopReason::Aborted).into(),
            },
            None,
            &mut changes,
        );
        transcript.apply(
            &AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
            None,
            &mut changes,
        );
        assert!(changes.finished);
        assert_eq!(transcript.failed_response(), Some(2));
    }
}
