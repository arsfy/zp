//! Streaming events and helpers to build and consume them.
//!
//! A response stream is a sequence of [`AssistantMessageEvent`]s that always ends with
//! exactly one `Done` or `Error`. Folding the events with a [`StreamAccumulator`] yields
//! the partial message at any point; providers use the same accumulator to build the final
//! message, so the two can never disagree.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use futures::channel::mpsc;
use futures::{Stream, StreamExt as _};
use serde_json::Value;

use crate::partial_json::parse_partial_json;
use crate::types::{
    AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, ToolCall, Usage,
};

/// One step of a streaming response.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistantMessageEvent {
    /// The response started. Carries the empty message skeleton (api, provider, model).
    Start {
        /// Message skeleton without content.
        message: AssistantMessage,
    },
    /// The provider's latest cumulative usage for this response.
    Usage { usage: Usage },
    /// A text block opened at `index`.
    TextStart {
        /// Content index.
        index: usize,
    },
    /// Text was appended to the block at `index`.
    TextDelta {
        /// Content index.
        index: usize,
        /// Appended text.
        delta: String,
    },
    /// The text block at `index` is complete.
    TextEnd {
        /// Content index.
        index: usize,
    },
    /// A thinking block opened at `index`.
    ThinkingStart {
        /// Content index.
        index: usize,
        /// Whether the provider redacted this reasoning.
        redacted: bool,
    },
    /// Reasoning text was appended to the block at `index`.
    ThinkingDelta {
        /// Content index.
        index: usize,
        /// Appended text.
        delta: String,
    },
    /// The thinking block at `index` is complete.
    ThinkingEnd {
        /// Content index.
        index: usize,
        /// Replay data for the block, if any.
        signature: Option<String>,
    },
    /// A tool call opened at `index`.
    ToolCallStart {
        /// Content index.
        index: usize,
        /// Call id (may be empty until known).
        id: String,
        /// Tool name (may be empty until known).
        name: String,
    },
    /// Raw JSON was appended to the arguments of the call at `index`.
    ToolCallDelta {
        /// Content index.
        index: usize,
        /// Appended JSON text.
        delta: String,
    },
    /// The tool call at `index` is complete.
    ToolCallEnd {
        /// Content index.
        index: usize,
        /// The final call with fully parsed arguments.
        tool_call: ToolCall,
    },
    /// The response finished successfully.
    Done {
        /// The final message.
        message: AssistantMessage,
    },
    /// The response failed or was aborted. `message.stop_reason` is `Error` or `Aborted`
    /// and `message.error_message` explains why; content received so far is kept.
    Error {
        /// The final (partial) message.
        message: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// Whether this event ends the stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }
}

/// A boxed stream of response events.
pub type AssistantMessageStream = Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send>>;

/// Folds events into the message they describe.
#[derive(Clone, Debug)]
pub struct StreamAccumulator {
    message: AssistantMessage,
    raw_arguments: HashMap<usize, String>,
}

impl StreamAccumulator {
    /// Starts from a message skeleton.
    pub fn new(message: AssistantMessage) -> Self {
        Self {
            message,
            raw_arguments: HashMap::new(),
        }
    }

    /// The message as of the last applied event.
    pub fn message(&self) -> &AssistantMessage {
        &self.message
    }

    /// Mutable access for metadata the events do not carry (usage, ids, stop reason).
    pub fn message_mut(&mut self) -> &mut AssistantMessage {
        &mut self.message
    }

    /// Consumes the accumulator.
    pub fn into_message(self) -> AssistantMessage {
        self.message
    }

    /// Applies one event.
    pub fn apply(&mut self, event: &AssistantMessageEvent) {
        use AssistantMessageEvent as E;
        match event {
            E::Start { message } => {
                self.message = message.clone();
                self.raw_arguments.clear();
            }
            E::Usage { usage } => self.message.usage = usage.clone(),
            E::TextStart { index } => self.insert(
                *index,
                AssistantContent::Text(TextContent {
                    text: String::new(),
                }),
            ),
            E::TextDelta { index, delta } => {
                if let Some(AssistantContent::Text(text)) = self.message.content.get_mut(*index) {
                    text.text.push_str(delta);
                }
            }
            E::ThinkingStart { index, redacted } => self.insert(
                *index,
                AssistantContent::Thinking(ThinkingContent {
                    redacted: *redacted,
                    ..ThinkingContent::default()
                }),
            ),
            E::ThinkingDelta { index, delta } => {
                if let Some(AssistantContent::Thinking(thinking)) =
                    self.message.content.get_mut(*index)
                {
                    thinking.thinking.push_str(delta);
                }
            }
            E::ThinkingEnd { index, signature } => {
                if let Some(AssistantContent::Thinking(thinking)) =
                    self.message.content.get_mut(*index)
                {
                    thinking.signature.clone_from(signature);
                }
            }
            E::ToolCallStart { index, id, name } => {
                self.raw_arguments.insert(*index, String::new());
                self.insert(
                    *index,
                    AssistantContent::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: Value::Object(Default::default()),
                    }),
                );
            }
            E::ToolCallDelta { index, delta } => {
                let raw = self.raw_arguments.entry(*index).or_default();
                raw.push_str(delta);
                if let Some(AssistantContent::ToolCall(call)) = self.message.content.get_mut(*index)
                    && let Some(arguments) = parse_partial_json(raw)
                {
                    call.arguments = arguments;
                }
            }
            E::ToolCallEnd { index, tool_call } => {
                self.raw_arguments.remove(index);
                if let Some(slot) = self.message.content.get_mut(*index) {
                    *slot = AssistantContent::ToolCall(tool_call.clone());
                }
            }
            E::TextEnd { .. } => {}
            E::Done { message } | E::Error { message } => {
                self.message = message.clone();
                self.raw_arguments.clear();
            }
        }
    }

    fn insert(&mut self, index: usize, block: AssistantContent) {
        if index == self.message.content.len() {
            self.message.content.push(block);
        } else if let Some(slot) = self.message.content.get_mut(index) {
            *slot = block;
        } else {
            tracing::warn!(index, "out-of-order content block in stream");
            self.message.content.push(block);
        }
    }
}

/// Sends events to a stream while keeping an accumulator in sync. Providers emit through
/// this so the final message always equals the folded events.
pub struct EventSink {
    sender: mpsc::UnboundedSender<AssistantMessageEvent>,
    accumulator: StreamAccumulator,
}

impl EventSink {
    /// The message built so far.
    pub fn message(&self) -> &AssistantMessage {
        self.accumulator.message()
    }

    /// Mutable access for usage, stop reason, response id and similar metadata.
    pub fn message_mut(&mut self) -> &mut AssistantMessage {
        self.accumulator.message_mut()
    }

    /// Number of content blocks so far (the index the next block will get).
    pub fn next_index(&self) -> usize {
        self.accumulator.message().content.len()
    }

    /// Applies and forwards an event. Returns `false` if the consumer went away.
    pub fn emit(&mut self, event: AssistantMessageEvent) -> bool {
        self.accumulator.apply(&event);
        self.sender.unbounded_send(event).is_ok()
    }

    /// Forwards usage metadata as soon as the provider reports it.
    pub(crate) fn emit_usage(&mut self) {
        self.emit(AssistantMessageEvent::Usage {
            usage: self.message().usage.clone(),
        });
    }

    /// Emits `Done` with the current message.
    pub fn done(mut self) {
        let message = self.accumulator.message().clone();
        self.emit(AssistantMessageEvent::Done { message });
    }

    /// Emits `Error` with the current message, marking it failed or aborted.
    pub fn fail(mut self, reason: StopReason, error: impl Into<String>) {
        let message = {
            let message = self.accumulator.message_mut();
            message.stop_reason = reason;
            message.error_message = Some(error.into());
            message.clone()
        };
        self.emit(AssistantMessageEvent::Error { message });
    }
}

/// Builds a lazy event stream from a producer future. Nothing runs until the stream is
/// polled; dropping the stream cancels the producer.
pub fn event_stream<F, Fut>(skeleton: AssistantMessage, producer: F) -> AssistantMessageStream
where
    F: FnOnce(EventSink) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (sender, receiver) = mpsc::unbounded();
    let sink = EventSink {
        sender,
        accumulator: StreamAccumulator::new(skeleton),
    };
    let driver = futures::stream::once(producer(sink)).filter_map(|()| async { None });
    Box::pin(futures::stream::select(receiver, driver))
}

/// A stream that immediately fails with `message`.
pub fn error_stream(
    skeleton: AssistantMessage,
    error: impl Into<String>,
) -> AssistantMessageStream {
    let error = error.into();
    event_stream(skeleton, move |sink| async move {
        sink.fail(StopReason::Error, error);
    })
}

/// A stream that immediately ends as aborted.
pub(crate) fn aborted_stream(skeleton: AssistantMessage) -> AssistantMessageStream {
    event_stream(skeleton, move |sink| async move {
        sink.fail(StopReason::Aborted, "Request was aborted");
    })
}

/// Drains a stream and returns its final message. A stream that ends without a terminal
/// event yields an error message.
pub async fn collect_message(mut stream: AssistantMessageStream) -> AssistantMessage {
    let mut accumulator: Option<StreamAccumulator> = None;
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Done { message } | AssistantMessageEvent::Error { message } => {
                return message;
            }
            AssistantMessageEvent::Start { message } => {
                accumulator = Some(StreamAccumulator::new(message));
            }
            other => {
                if let Some(accumulator) = accumulator.as_mut() {
                    accumulator.apply(&other);
                }
            }
        }
    }
    let mut message = accumulator
        .map(StreamAccumulator::into_message)
        .unwrap_or_else(|| AssistantMessage::empty("", "", ""));
    message.stop_reason = StopReason::Error;
    message.error_message = Some("stream ended without a final event".into());
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn skeleton() -> AssistantMessage {
        AssistantMessage::empty("faux", "p", "m")
    }

    #[test]
    fn accumulator_builds_partial_tool_arguments() {
        let mut acc = StreamAccumulator::new(skeleton());
        acc.apply(&AssistantMessageEvent::ToolCallStart {
            index: 0,
            id: "c1".into(),
            name: "read".into(),
        });
        acc.apply(&AssistantMessageEvent::ToolCallDelta {
            index: 0,
            delta: r#"{"path": "src/m"#.into(),
        });
        let AssistantContent::ToolCall(call) = &acc.message().content[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.arguments, json!({"path": "src/m"}));
    }

    #[tokio::test]
    async fn event_stream_delivers_events_then_done() {
        let stream = event_stream(skeleton(), |mut sink| async move {
            let index = sink.next_index();
            sink.emit(AssistantMessageEvent::TextStart { index });
            sink.emit(AssistantMessageEvent::TextDelta {
                index,
                delta: "hello".into(),
            });
            sink.emit(AssistantMessageEvent::TextEnd { index });
            sink.done();
        });
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 4);
        let AssistantMessageEvent::Done { message } = events.last().unwrap() else {
            panic!("expected done");
        };
        assert_eq!(message.text(), "hello");
    }

    #[tokio::test]
    async fn collect_reports_errors() {
        let message = collect_message(error_stream(skeleton(), "boom")).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn stream_without_terminal_event_is_an_error() {
        let stream = event_stream(skeleton(), |_sink| async {});
        let message = collect_message(stream).await;
        assert_eq!(message.stop_reason, StopReason::Error);
    }
}
