//! A scripted in-process provider for tests and offline development.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::event::{AssistantMessageEvent, AssistantMessageStream, event_stream};
use crate::model::{Api, InputModality, Model, ModelCost};
use crate::options::StreamOptions;
use crate::provider::Provider;
use crate::types::{AssistantMessage, Context, StopReason, ToolCall, Usage};

/// One block of a scripted response.
#[derive(Clone, Debug, PartialEq)]
pub enum FauxBlock {
    /// Visible text.
    Text(String),
    /// Reasoning text.
    Thinking(String),
    /// A tool call.
    ToolCall {
        /// Tool name.
        name: String,
        /// Arguments object.
        arguments: Value,
    },
}

/// A scripted response.
#[derive(Clone, Debug, PartialEq)]
pub enum FauxResponse {
    /// A successful response.
    Message {
        /// Content blocks in order.
        blocks: Vec<FauxBlock>,
        /// Stop reason; defaults to `ToolUse` when tool calls are present.
        stop_reason: Option<StopReason>,
        /// Reported usage.
        usage: Usage,
    },
    /// A failed request with the given message.
    Error(String),
}

impl FauxResponse {
    /// A text-only response.
    pub fn text(text: impl Into<String>) -> Self {
        Self::blocks(vec![FauxBlock::Text(text.into())])
    }

    /// A response with a single tool call.
    pub fn tool_call(name: impl Into<String>, arguments: Value) -> Self {
        Self::blocks(vec![FauxBlock::ToolCall {
            name: name.into(),
            arguments,
        }])
    }

    /// A response with the given blocks.
    pub fn blocks(blocks: Vec<FauxBlock>) -> Self {
        Self::Message {
            blocks,
            stop_reason: None,
            usage: Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..Usage::default()
            },
        }
    }
}

#[derive(Default)]
struct FauxState {
    responses: VecDeque<FauxResponse>,
    requests: Vec<(Context, StreamOptions)>,
    next_call_id: u64,
}

/// Replays queued [`FauxResponse`]s in order and records every request.
#[derive(Clone)]
pub struct FauxProvider {
    id: String,
    models: Arc<[Model]>,
    state: Arc<Mutex<FauxState>>,
    chunk_delay: Duration,
}

impl Default for FauxProvider {
    fn default() -> Self {
        Self::new("faux")
    }
}

impl FauxProvider {
    /// Creates a provider with one model, `faux-1`.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        let model = Model {
            id: "faux-1".into(),
            name: "Faux 1".into(),
            api: Api::Faux,
            provider: id.clone(),
            base_url: String::new(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                ..ModelCost::default()
            },
            context_window: 128_000,
            max_tokens: 8_192,
            thinking_level_map: None,
            headers: HashMap::new(),
            compat: Default::default(),
        };
        Self {
            id,
            models: vec![model].into(),
            state: Arc::default(),
            chunk_delay: Duration::ZERO,
        }
    }

    /// Delays every streamed chunk, to exercise cancellation and UI streaming.
    pub fn with_chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self
    }

    /// The provider's single model.
    pub fn model(&self) -> Model {
        self.models[0].clone()
    }

    /// Queues a response.
    pub fn push(&self, response: FauxResponse) {
        self.lock().responses.push_back(response);
    }

    /// Contexts of all requests received so far.
    pub fn requests(&self) -> Vec<Context> {
        self.lock()
            .requests
            .iter()
            .map(|(context, _)| context.clone())
            .collect()
    }

    /// Number of queued responses not yet consumed.
    pub fn pending(&self) -> usize {
        self.lock().responses.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FauxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Splits text into a few chunks so streaming paths are exercised.
fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let size = chars.len().div_ceil(3).max(1);
    chars
        .chunks(size)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

impl Provider for FauxProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        "Faux"
    }

    fn models(&self) -> Arc<[Model]> {
        self.models.clone()
    }

    fn requires_api_key(&self) -> bool {
        false
    }

    fn default_model_id(&self) -> Option<&str> {
        Some("faux-1")
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let (response, first_call_id) = {
            let mut state = self.lock();
            state.requests.push((context.clone(), options.clone()));
            let response = state.responses.pop_front();
            let first = state.next_call_id;
            if let Some(FauxResponse::Message { blocks, .. }) = &response {
                state.next_call_id += blocks
                    .iter()
                    .filter(|block| matches!(block, FauxBlock::ToolCall { .. }))
                    .count() as u64;
            }
            (response, first)
        };
        let model = model.clone();
        let delay = self.chunk_delay;
        let cancel = options.cancel.clone();
        let skeleton = AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);

        event_stream(skeleton, move |mut sink| async move {
            let pause = || async {
                if delay.is_zero() {
                    tokio::task::yield_now().await;
                    return !cancel.is_cancelled();
                }
                tokio::select! {
                    () = cancel.cancelled() => false,
                    () = tokio::time::sleep(delay) => true,
                }
            };
            let Some(response) = response else {
                return sink.fail(StopReason::Error, "no faux response queued");
            };
            let (blocks, stop_reason, usage) = match response {
                FauxResponse::Error(message) => return sink.fail(StopReason::Error, message),
                FauxResponse::Message {
                    blocks,
                    stop_reason,
                    usage,
                } => (blocks, stop_reason, usage),
            };
            let skeleton = sink.message().clone();
            sink.emit(AssistantMessageEvent::Start { message: skeleton });

            let mut call_id = first_call_id;
            let mut has_tool_calls = false;
            for block in blocks {
                let index = sink.next_index();
                match block {
                    FauxBlock::Text(text) => {
                        sink.emit(AssistantMessageEvent::TextStart { index });
                        for delta in chunks(&text) {
                            if !pause().await {
                                return sink.fail(StopReason::Aborted, "Request was aborted");
                            }
                            sink.emit(AssistantMessageEvent::TextDelta { index, delta });
                        }
                        sink.emit(AssistantMessageEvent::TextEnd { index });
                    }
                    FauxBlock::Thinking(text) => {
                        sink.emit(AssistantMessageEvent::ThinkingStart {
                            index,
                            redacted: false,
                        });
                        for delta in chunks(&text) {
                            if !pause().await {
                                return sink.fail(StopReason::Aborted, "Request was aborted");
                            }
                            sink.emit(AssistantMessageEvent::ThinkingDelta { index, delta });
                        }
                        sink.emit(AssistantMessageEvent::ThinkingEnd {
                            index,
                            signature: None,
                        });
                    }
                    FauxBlock::ToolCall { name, arguments } => {
                        has_tool_calls = true;
                        let id = format!("faux_call_{call_id}");
                        call_id += 1;
                        sink.emit(AssistantMessageEvent::ToolCallStart {
                            index,
                            id: id.clone(),
                            name: name.clone(),
                        });
                        for delta in chunks(&arguments.to_string()) {
                            if !pause().await {
                                return sink.fail(StopReason::Aborted, "Request was aborted");
                            }
                            sink.emit(AssistantMessageEvent::ToolCallDelta { index, delta });
                        }
                        sink.emit(AssistantMessageEvent::ToolCallEnd {
                            index,
                            tool_call: ToolCall {
                                id,
                                name,
                                arguments,
                            },
                        });
                    }
                }
            }
            let message = sink.message_mut();
            message.usage = usage;
            message.usage.update_total();
            model.calculate_cost(&mut message.usage);
            message.stop_reason = stop_reason.unwrap_or(if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            });
            sink.done();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{StreamAccumulator, collect_message};
    use crate::types::{AssistantContent, Message};
    use futures::StreamExt as _;
    use serde_json::json;

    #[tokio::test]
    async fn replays_all_block_kinds_and_records_requests() {
        let faux = FauxProvider::default();
        faux.push(FauxResponse::blocks(vec![
            FauxBlock::Thinking("Let me think.".into()),
            FauxBlock::Text("Hello there!".into()),
            FauxBlock::ToolCall {
                name: "read".into(),
                arguments: json!({"path": "a.rs"}),
            },
        ]));
        let context = Context {
            messages: vec![Message::user("hi")],
            ..Context::default()
        };
        let events: Vec<_> = faux
            .stream(&faux.model(), &context, StreamOptions::default())
            .collect()
            .await;

        let kinds: Vec<&str> = events
            .iter()
            .map(|event| match event {
                AssistantMessageEvent::Start { .. } => "start",
                AssistantMessageEvent::Usage { .. } => "usage",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
                AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
                AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
                AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
                AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
                AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
                AssistantMessageEvent::Done { .. } => "done",
                AssistantMessageEvent::Error { .. } => "error",
            })
            .collect();
        for kind in [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done",
        ] {
            assert!(kinds.contains(&kind), "missing {kind}");
        }
        assert_eq!(kinds.first(), Some(&"start"));
        assert_eq!(kinds.last(), Some(&"done"));

        let AssistantMessageEvent::Done { message } = events.last().unwrap() else {
            panic!()
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.text(), "Hello there!");
        assert!(
            matches!(&message.content[0], AssistantContent::Thinking(t) if t.thinking == "Let me think.")
        );
        assert_eq!(
            message.tool_calls().next().unwrap().arguments,
            json!({"path": "a.rs"})
        );
        assert!((message.usage.cost.total - (10.0 + 10.0) / 1_000_000.0).abs() < 1e-12);

        let mut accumulator = StreamAccumulator::new(AssistantMessage::empty("", "", ""));
        for event in &events[..events.len() - 1] {
            accumulator.apply(event);
        }
        assert_eq!(accumulator.message().content, message.content);
        assert_eq!(faux.requests().len(), 1);
    }

    #[tokio::test]
    async fn errors_and_empty_queue() {
        let faux = FauxProvider::default();
        faux.push(FauxResponse::Error("boom".into()));
        let message = collect_message(faux.stream(
            &faux.model(),
            &Context::default(),
            StreamOptions::default(),
        ))
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("boom"));

        let message = collect_message(faux.stream(
            &faux.model(),
            &Context::default(),
            StreamOptions::default(),
        ))
        .await;
        assert_eq!(
            message.error_message.as_deref(),
            Some("no faux response queued")
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_and_keeps_partial_content() {
        let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(20));
        faux.push(FauxResponse::text(
            "a fairly long answer that streams slowly",
        ));
        let options = StreamOptions::default();
        let cancel = options.cancel.clone();
        let mut stream = faux.stream(&faux.model(), &Context::default(), options);
        let mut last = None;
        while let Some(event) = stream.next().await {
            if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
                cancel.cancel();
            }
            last = Some(event);
        }
        let Some(AssistantMessageEvent::Error { message }) = last else {
            panic!("expected error")
        };
        assert_eq!(message.stop_reason, StopReason::Aborted);
        assert!(!message.text().is_empty());
    }

    #[tokio::test]
    async fn tool_call_ids_are_unique_across_responses() {
        let faux = FauxProvider::default();
        faux.push(FauxResponse::tool_call("a", json!({})));
        faux.push(FauxResponse::tool_call("b", json!({})));
        let first = collect_message(faux.stream(
            &faux.model(),
            &Context::default(),
            StreamOptions::default(),
        ))
        .await;
        let second = collect_message(faux.stream(
            &faux.model(),
            &Context::default(),
            StreamOptions::default(),
        ))
        .await;
        assert_ne!(
            first.tool_calls().next().unwrap().id,
            second.tool_calls().next().unwrap().id
        );
    }
}
