//! The displayed transcript: rows built from agent messages and kept current from agent
//! events, independent of rendering.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde_json::Value;
use zp_agent::{AgentEvent, AgentMessage};
use zp_ai::{
    AssistantMessage, AssistantMessageEvent, Message, StopReason, StreamAccumulator, UserContent,
};
use zp_core::UsageTotals;

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

/// One rendered row of the transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// A user message.
    User {
        text: String,
        images: usize,
        /// Files attached with `@path`.
        files: Vec<String>,
        ordinal: usize,
    },
    /// A model response with its tool calls. While streaming, the current content is
    /// read through [`Transcript::assistant`].
    Assistant {
        message: AssistantMessage,
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
}

fn text_of(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(UserContent::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Transcript {
    /// A transcript showing `messages` (a restored conversation).
    pub fn from_messages(messages: &[AgentMessage]) -> Self {
        let mut transcript = Self::default();
        let mut changes = Changes::default();
        for message in messages {
            transcript.finish_message(message, &mut changes);
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
    fn finish_message(&mut self, message: &AgentMessage, changes: &mut Changes) {
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        match message {
            AgentMessage::Llm(Message::User(user)) => {
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
                        ordinal,
                    },
                    changes,
                );
            }
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                self.active_thinking.clear();
                let row = match self.streaming.take() {
                    Some((row, _)) => {
                        self.rows[row] = Row::Assistant {
                            message: assistant.clone(),
                            ordinal,
                        };
                        changes.dirty.insert(row);
                        row
                    }
                    None => self.push(
                        Row::Assistant {
                            message: assistant.clone(),
                            ordinal,
                        },
                        changes,
                    ),
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
    pub fn apply(&mut self, event: &AgentEvent, changes: &mut Changes) {
        match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Llm(Message::Assistant(message)),
            } => {
                self.active_thinking.clear();
                let row = self.push(
                    Row::Assistant {
                        message: message.clone(),
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
            AgentEvent::MessageEnd { message } => self.finish_message(message, changes),
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
                        message: accumulator.into_message(),
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
        let transcript = Transcript::from_messages(&messages);
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
            &mut changes,
        );
        transcript.apply(
            &AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::ThinkingStart {
                    index: 0,
                    redacted: false,
                },
            },
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
                &mut changes,
            );
            transcript.apply(
                &AgentEvent::MessageUpdate {
                    event: AssistantMessageEvent::ThinkingStart {
                        index: 0,
                        redacted: false,
                    },
                },
                &mut changes,
            );
            assert!(transcript.thinking_is_running(0, 0));
            transcript.apply(&end, &mut changes);
            assert!(!transcript.thinking_is_running(0, 0));
            transcript.apply(
                &AgentEvent::MessageStart {
                    message: skeleton.clone().into(),
                },
                &mut changes,
            );
            assert!(!transcript.thinking_is_running(1, 0));
        }
    }

    #[test]
    fn streaming_usage_is_replaced_by_final_usage_without_double_counting() {
        let first = assistant(Vec::new(), StopReason::Stop);
        let mut transcript = Transcript::from_messages(&[first.clone().into()]);
        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::MessageStart {
                message: AssistantMessage::empty("faux", "faux", "m").into(),
            },
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
            &mut changes,
        );
        assert!(transcript.streaming_usage().is_none());
        assert_eq!(transcript.usage().output, 25);
        assert_eq!(transcript.usage().cache_read, 90);
    }

    #[test]
    fn streams_and_tracks_tools() {
        let mut transcript = Transcript::from_messages(&[AgentMessage::user("go")]);
        let mut changes = Changes::default();
        let skeleton = AssistantMessage::empty("faux", "faux", "m");
        transcript.apply(
            &AgentEvent::MessageStart {
                message: skeleton.clone().into(),
            },
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
            transcript.apply(&AgentEvent::MessageUpdate { event }, &mut changes);
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
            &mut changes,
        );
        transcript.apply(
            &AgentEvent::ToolExecutionUpdate {
                tool_call_id: "c1".into(),
                tool_name: "bash".into(),
                partial_result: AgentToolResult::text("partial"),
            },
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
            &mut changes,
        );
        assert_eq!(transcript.tool("c1").unwrap().status, ToolStatus::Error);
        assert_eq!(changes.dirty.iter().copied().collect::<Vec<_>>(), [1]);

        let mut changes = Changes::default();
        transcript.apply(
            &AgentEvent::MessageEnd {
                message: assistant(Vec::new(), StopReason::Aborted).into(),
            },
            &mut changes,
        );
        transcript.apply(
            &AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
            &mut changes,
        );
        assert!(changes.finished);
        assert_eq!(transcript.failed_response(), Some(2));
    }
}
