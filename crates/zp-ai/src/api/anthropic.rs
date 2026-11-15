//! Anthropic Messages API (`POST {base}/v1/messages`, SSE).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{object_schema, parse_arguments};
use crate::event::{AssistantMessageEvent, AssistantMessageStream, EventSink, event_stream};
use crate::http::{self, HttpError};
use crate::model::{Model, ThinkingLevel};
use crate::options::{DEFAULT_MAX_RETRIES, StreamOptions};
use crate::transform::{clamp_max_tokens, prepare_messages, replace_images};
use crate::types::{
    AssistantContent, AssistantMessage, Context, Message, StopReason, ToolCall, UserContent,
};

pub(crate) const API_VERSION: &str = "2023-06-01";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
/// Output tokens always left for the answer when a thinking budget shares `max_tokens`.
const MIN_ANSWER_TOKENS: u64 = 1024;

/// Starts a streaming request.
pub(crate) fn stream(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageStream {
    let skeleton = AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);
    event_stream(skeleton, move |sink| run(model, context, options, sink))
}

async fn run(model: Model, context: Context, options: StreamOptions, mut sink: EventSink) {
    let request = build_request(&model, &context, &options);
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let api_key = options.api_key.clone().unwrap_or_default();
    let mut headers: Vec<(&str, &str)> = vec![
        ("x-api-key", api_key.as_str()),
        ("anthropic-version", API_VERSION),
    ];
    if request.interleaved_thinking {
        headers.push(("anthropic-beta", INTERLEAVED_THINKING_BETA));
    }
    headers.extend(model.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    headers.extend(
        options
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );

    let response = match http::post_json(
        &url,
        http::header_map(headers),
        &request.body,
        options.max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
        &options.cancel,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => return fail(sink, error),
    };

    let skeleton = sink.message().clone();
    sink.emit(AssistantMessageEvent::Start { message: skeleton });
    let mut decoder = Decoder::new(&model);
    let result = http::drive_sse(response, &options.cancel, |event| {
        decoder.handle(event.event.as_deref(), &event.data, &mut sink)
    })
    .await;
    if let Err(error) = result {
        decoder.close_open_blocks(&mut sink);
        return fail(sink, error);
    }
    match decoder.finish(&mut sink) {
        Ok(()) => sink.done(),
        Err(error) => sink.fail(StopReason::Error, error),
    }
}

fn fail(sink: EventSink, error: HttpError) {
    match error {
        HttpError::Cancelled => sink.fail(StopReason::Aborted, "Request was aborted"),
        HttpError::Failed(message) => sink.fail(StopReason::Error, message),
    }
}

/// A request body plus the headers it implies.
pub(crate) struct Request {
    pub(crate) body: Value,
    pub(crate) interleaved_thinking: bool,
}

/// Anthropic tool ids must match `^[a-zA-Z0-9_-]{1,64}$`.
fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn effort(model: &Model, level: ThinkingLevel) -> String {
    match model.thinking_level_value(level) {
        Some(Some(value)) => value.to_owned(),
        _ => match level {
            ThinkingLevel::Minimal | ThinkingLevel::Low => "low".into(),
            ThinkingLevel::Medium => "medium".into(),
            _ => "high".into(),
        },
    }
}

/// Builds the request body. Pure, for testing.
pub(crate) fn build_request(model: &Model, context: &Context, options: &StreamOptions) -> Request {
    let cache_control = json!({"type": "ephemeral"});
    let compat = &model.compat;
    let level = if model.reasoning {
        model.clamp_thinking_level(options.thinking)
    } else {
        ThinkingLevel::Off
    };
    let thinking_enabled = level != ThinkingLevel::Off;
    let adaptive = compat.force_adaptive_thinking == Some(true);

    let mut max_tokens = clamp_max_tokens(
        model,
        context,
        options.max_tokens.unwrap_or(model.max_tokens).max(1),
    );

    let mut body = Map::new();
    body.insert("model".into(), Value::from(model.id.clone()));
    body.insert("stream".into(), Value::Bool(true));

    if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        body.insert(
            "system".into(),
            json!([{"type": "text", "text": system, "cache_control": cache_control}]),
        );
    }

    let mut interleaved_thinking = false;
    if model.reasoning {
        if !thinking_enabled {
            body.insert("thinking".into(), json!({"type": "disabled"}));
        } else if adaptive {
            body.insert(
                "thinking".into(),
                json!({"type": "adaptive", "display": "summarized"}),
            );
            body.insert(
                "output_config".into(),
                json!({"effort": effort(model, level)}),
            );
        } else {
            let budgets = options.thinking_budgets.unwrap_or_default();
            let mut budget = budgets.for_level(level);
            if options.max_tokens.is_some() {
                max_tokens = (max_tokens + budget).min(model.max_tokens.max(max_tokens));
            }
            budget = budget
                .min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS))
                .max(1024);
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget, "display": "summarized"}),
            );
            interleaved_thinking = true;
        }
    }
    body.insert("max_tokens".into(), Value::from(max_tokens));

    if let Some(temperature) = options.temperature
        && !thinking_enabled
        && compat.supports_temperature != Some(false)
    {
        body.insert("temperature".into(), json!(temperature));
    }

    if !context.tools.is_empty() {
        let eager = compat.supports_eager_tool_input_streaming != Some(false);
        let last = context.tools.len() - 1;
        let tools: Vec<Value> = context
            .tools
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                let mut definition = json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": object_schema(&tool.parameters),
                });
                if eager {
                    definition["eager_input_streaming"] = Value::Bool(true);
                }
                if index == last {
                    definition["cache_control"] = cache_control.clone();
                }
                definition
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    let messages = prepare_messages(&context.messages, model, normalize_tool_call_id);
    let mut converted = convert_messages(&messages, compat.allow_empty_signature == Some(true));
    add_cache_control_to_last_user_message(&mut converted, &cache_control);
    body.insert("messages".into(), Value::Array(converted));

    Request {
        body: Value::Object(body),
        interleaved_thinking,
    }
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}})
}

fn user_blocks(content: &[UserContent]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            UserContent::Text(text) if text.text.trim().is_empty() => None,
            UserContent::Text(text) => Some(json!({"type": "text", "text": text.text})),
            UserContent::Image(image) => Some(image_block(&image.data, &image.mime_type)),
        })
        .collect()
}

/// Tool result content: a string when text-only, blocks when images are present.
fn tool_result_content(content: &[UserContent]) -> Value {
    if content
        .iter()
        .all(|block| matches!(block, UserContent::Text(_)))
    {
        let text: Vec<&str> = content.iter().filter_map(UserContent::as_text).collect();
        return Value::from(text.join("\n"));
    }
    let mut blocks = user_blocks(content);
    if !blocks.iter().any(|block| block["type"] == "text") {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    Value::Array(blocks)
}

fn convert_messages(messages: &[Message], allow_empty_signature: bool) -> Vec<Value> {
    let mut output: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(user) => {
                let blocks = user_blocks(&user.content);
                if !blocks.is_empty() {
                    output.push(json!({"role": "user", "content": blocks}));
                }
            }
            Message::Assistant(assistant) => {
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if !text.text.trim().is_empty() {
                                blocks.push(json!({"type": "text", "text": text.text}));
                            }
                        }
                        AssistantContent::Thinking(thinking) => {
                            let signature = thinking.signature.as_deref().unwrap_or_default();
                            if thinking.redacted {
                                blocks.push(json!({"type": "redacted_thinking", "data": signature}));
                            } else if !signature.trim().is_empty() {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking.thinking,
                                    "signature": signature,
                                }));
                            } else if !thinking.thinking.trim().is_empty() {
                                // Unsigned reasoning (e.g. from an aborted stream) cannot be
                                // replayed as thinking.
                                blocks.push(if allow_empty_signature {
                                    json!({"type": "thinking", "thinking": thinking.thinking, "signature": ""})
                                } else {
                                    json!({"type": "text", "text": thinking.thinking})
                                });
                            }
                        }
                        AssistantContent::ToolCall(call) => blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": if call.arguments.is_object() { call.arguments.clone() } else { json!({}) },
                        })),
                    }
                }
                if !blocks.is_empty() {
                    output.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::ToolResult(_) => {
                // Consecutive tool results share one user message.
                let mut blocks = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    let content = if result.is_error {
                        // Anthropic rejects images inside error results.
                        replace_images(result.content.clone(), "(image omitted from error result)")
                    } else {
                        result.content.clone()
                    };
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": tool_result_content(&content),
                        "is_error": result.is_error,
                    }));
                    index += 1;
                }
                output.push(json!({"role": "user", "content": blocks}));
                continue;
            }
        }
        index += 1;
    }
    output
}

fn add_cache_control_to_last_user_message(messages: &mut [Value], cache_control: &Value) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if last["role"] != "user" {
        return;
    }
    if let Some(block) = last["content"]
        .as_array_mut()
        .and_then(|blocks| blocks.last_mut())
        && let Some(object) = block.as_object_mut()
    {
        object.insert("cache_control".into(), cache_control.clone());
    }
}

/// Per-block stream state, keyed by Anthropic's block index.
enum OpenBlock {
    Text {
        index: usize,
    },
    Thinking {
        index: usize,
        signature: String,
    },
    ToolCall {
        index: usize,
        id: String,
        name: String,
        raw: String,
    },
}

/// Turns Anthropic SSE events into ZP events.
pub(crate) struct Decoder<'a> {
    model: &'a Model,
    blocks: HashMap<u64, OpenBlock>,
    stop_reason: Option<StopReason>,
    error: Option<String>,
    started: bool,
    stopped: bool,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(model: &'a Model) -> Self {
        Self {
            model,
            blocks: HashMap::new(),
            stop_reason: None,
            error: None,
            started: false,
            stopped: false,
        }
    }

    fn update_usage(&self, sink: &mut EventSink, usage: &Value) {
        let message = sink.message_mut();
        let read = |key: &str| usage.get(key).and_then(Value::as_u64);
        if let Some(value) = read("input_tokens") {
            message.usage.input = value;
        }
        if let Some(value) = read("output_tokens") {
            message.usage.output = value;
        }
        if let Some(value) = read("cache_read_input_tokens") {
            message.usage.cache_read = value;
        }
        if let Some(value) = read("cache_creation_input_tokens") {
            message.usage.cache_write = value;
        }
        if let Some(value) = usage
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Value::as_u64)
        {
            message.usage.reasoning = value;
        }
        message.usage.update_total();
        self.model.calculate_cost(&mut message.usage);
    }

    /// Handles one SSE event.
    pub(crate) fn handle(
        &mut self,
        event_name: Option<&str>,
        data: &str,
        sink: &mut EventSink,
    ) -> Result<(), String> {
        if event_name == Some("error") {
            let message = serde_json::from_str::<Value>(data)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| data.to_owned());
            return Err(message);
        }
        if data.trim().is_empty() {
            return Ok(());
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| format!("could not parse Anthropic event: {error}; data={data}"))?;
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                self.started = true;
                if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                    sink.message_mut().response_id = Some(id.to_owned());
                }
                if let Some(usage) = event.pointer("/message/usage") {
                    self.update_usage(sink, usage);
                }
            }
            "content_block_start" => self.start_block(&event, sink),
            "content_block_delta" => self.delta(&event, sink),
            "content_block_stop" => {
                if let Some(key) = event["index"].as_u64()
                    && let Some(block) = self.blocks.remove(&key)
                {
                    Self::end_block(block, sink);
                }
            }
            "message_delta" => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    let (stop_reason, error) = map_stop_reason(reason, &event["delta"]);
                    self.stop_reason = Some(stop_reason);
                    self.error = error;
                }
                if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
                    self.update_usage(sink, usage);
                }
            }
            "message_stop" => self.stopped = true,
            "error" => {
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown provider error");
                return Err(message.to_owned());
            }
            _ => {}
        }
        Ok(())
    }

    fn start_block(&mut self, event: &Value, sink: &mut EventSink) {
        let Some(key) = event["index"].as_u64() else {
            return;
        };
        let block = &event["content_block"];
        let index = sink.next_index();
        let open = match block["type"].as_str().unwrap_or_default() {
            "text" => {
                sink.emit(AssistantMessageEvent::TextStart { index });
                if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                    sink.emit(AssistantMessageEvent::TextDelta {
                        index,
                        delta: text.to_owned(),
                    });
                }
                OpenBlock::Text { index }
            }
            "thinking" => {
                sink.emit(AssistantMessageEvent::ThinkingStart {
                    index,
                    redacted: false,
                });
                if let Some(text) = block["thinking"].as_str().filter(|t| !t.is_empty()) {
                    sink.emit(AssistantMessageEvent::ThinkingDelta {
                        index,
                        delta: text.to_owned(),
                    });
                }
                OpenBlock::Thinking {
                    index,
                    signature: block["signature"].as_str().unwrap_or_default().to_owned(),
                }
            }
            "redacted_thinking" => {
                sink.emit(AssistantMessageEvent::ThinkingStart {
                    index,
                    redacted: true,
                });
                sink.emit(AssistantMessageEvent::ThinkingDelta {
                    index,
                    delta: "[Reasoning redacted]".into(),
                });
                OpenBlock::Thinking {
                    index,
                    signature: block["data"].as_str().unwrap_or_default().to_owned(),
                }
            }
            "tool_use" => {
                let id = block["id"].as_str().unwrap_or_default().to_owned();
                let name = block["name"].as_str().unwrap_or_default().to_owned();
                sink.emit(AssistantMessageEvent::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                });
                let raw = match block.get("input") {
                    Some(input) if input.as_object().is_some_and(|o| !o.is_empty()) => {
                        input.to_string()
                    }
                    _ => String::new(),
                };
                OpenBlock::ToolCall {
                    index,
                    id,
                    name,
                    raw,
                }
            }
            other => {
                tracing::debug!(block_type = other, "ignoring unsupported content block");
                return;
            }
        };
        self.blocks.insert(key, open);
    }

    fn delta(&mut self, event: &Value, sink: &mut EventSink) {
        let Some(open) = event["index"]
            .as_u64()
            .and_then(|key| self.blocks.get_mut(&key))
        else {
            return;
        };
        let delta = &event["delta"];
        match (delta["type"].as_str().unwrap_or_default(), open) {
            ("text_delta", OpenBlock::Text { index }) => {
                let text = delta["text"].as_str().unwrap_or_default().to_owned();
                sink.emit(AssistantMessageEvent::TextDelta {
                    index: *index,
                    delta: text,
                });
            }
            ("thinking_delta", OpenBlock::Thinking { index, .. }) => {
                let text = delta["thinking"].as_str().unwrap_or_default().to_owned();
                sink.emit(AssistantMessageEvent::ThinkingDelta {
                    index: *index,
                    delta: text,
                });
            }
            ("signature_delta", OpenBlock::Thinking { signature, .. }) => {
                signature.push_str(delta["signature"].as_str().unwrap_or_default());
            }
            ("input_json_delta", OpenBlock::ToolCall { index, raw, .. }) => {
                let json = delta["partial_json"].as_str().unwrap_or_default();
                raw.push_str(json);
                sink.emit(AssistantMessageEvent::ToolCallDelta {
                    index: *index,
                    delta: json.to_owned(),
                });
            }
            _ => {}
        }
    }

    fn end_block(block: OpenBlock, sink: &mut EventSink) {
        match block {
            OpenBlock::Text { index } => {
                sink.emit(AssistantMessageEvent::TextEnd { index });
            }
            OpenBlock::Thinking { index, signature } => {
                sink.emit(AssistantMessageEvent::ThinkingEnd {
                    index,
                    signature: (!signature.is_empty()).then_some(signature),
                });
            }
            OpenBlock::ToolCall {
                index,
                id,
                name,
                raw,
            } => {
                sink.emit(AssistantMessageEvent::ToolCallEnd {
                    index,
                    tool_call: ToolCall {
                        id,
                        name,
                        arguments: parse_arguments(&raw),
                    },
                });
            }
        }
    }

    /// Ends blocks that were still open when the stream broke off, in content order.
    pub(crate) fn close_open_blocks(&mut self, sink: &mut EventSink) {
        let mut open: Vec<OpenBlock> = self.blocks.drain().map(|(_, block)| block).collect();
        open.sort_by_key(|block| match block {
            OpenBlock::Text { index }
            | OpenBlock::Thinking { index, .. }
            | OpenBlock::ToolCall { index, .. } => *index,
        });
        for block in open {
            Self::end_block(block, sink);
        }
    }

    /// Validates the end of the stream and records the stop reason.
    pub(crate) fn finish(&mut self, sink: &mut EventSink) -> Result<(), String> {
        self.close_open_blocks(sink);
        if self.started && !self.stopped {
            return Err("Anthropic stream ended before message_stop".into());
        }
        let Some(stop_reason) = self.stop_reason else {
            return Err("Anthropic stream ended without a stop reason".into());
        };
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        sink.message_mut().stop_reason = stop_reason;
        Ok(())
    }
}

fn map_stop_reason(reason: &str, delta: &Value) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => (StopReason::Stop, None),
        "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = delta
                .pointer("/stop_details/explanation")
                .and_then(Value::as_str)
                .unwrap_or("The model refused to complete the request");
            (StopReason::Error, Some(explanation.to_owned()))
        }
        other => (
            StopReason::Error,
            Some(format!("Provider stopped with: {other}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{StreamAccumulator, collect_message};
    use crate::model::{Api, test_model};
    use crate::options::ThinkingBudgets;
    use crate::types::{ImageContent, TextContent, ThinkingContent, Tool, ToolResultMessage};
    use futures::StreamExt as _;
    use std::collections::BTreeMap;

    fn context() -> Context {
        Context {
            system_prompt: Some("Be brief.".into()),
            messages: vec![Message::user("hello")],
            tools: vec![Tool {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
            }],
        }
    }

    #[test]
    fn builds_basic_request() {
        let model = test_model(Api::AnthropicMessages);
        let request = build_request(&model, &context(), &StreamOptions::default());
        let body = request.body;
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 64_000);
        assert_eq!(body["system"][0]["text"], "Be brief.");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["input_schema"]["required"][0], "path");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body.get("thinking").is_none());
        assert!(!request.interleaved_thinking);
    }

    #[test]
    fn budget_thinking_for_older_models() {
        let mut model = test_model(Api::AnthropicMessages);
        model.reasoning = true;
        let options = StreamOptions {
            thinking: ThinkingLevel::Medium,
            temperature: Some(0.5),
            ..StreamOptions::default()
        };
        let request = build_request(&model, &context(), &options);
        assert_eq!(request.body["thinking"]["type"], "enabled");
        assert_eq!(request.body["thinking"]["budget_tokens"], 8192);
        assert!(request.body.get("temperature").is_none());
        assert!(request.interleaved_thinking);

        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            max_tokens: Some(1000),
            thinking_budgets: Some(ThinkingBudgets {
                high: 4000,
                ..ThinkingBudgets::default()
            }),
            ..StreamOptions::default()
        };
        let body = build_request(&model, &context(), &options).body;
        assert_eq!(body["max_tokens"], 5000);
        assert_eq!(body["thinking"]["budget_tokens"], 3976);

        let body = build_request(&model, &context(), &StreamOptions::default()).body;
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    #[test]
    fn adaptive_thinking_uses_effort() {
        let mut model = test_model(Api::AnthropicMessages);
        model.reasoning = true;
        model.compat.force_adaptive_thinking = Some(true);
        model.thinking_level_map = Some(BTreeMap::from([
            (ThinkingLevel::Off, None),
            (ThinkingLevel::Max, Some("max".into())),
        ]));
        let options = StreamOptions {
            thinking: ThinkingLevel::Max,
            ..StreamOptions::default()
        };
        let body = build_request(&model, &context(), &options).body;
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");

        // `off` is unsupported here, so it clamps up to `minimal` -> effort "low".
        let body = build_request(&model, &context(), &StreamOptions::default()).body;
        assert_eq!(body["output_config"]["effort"], "low");
    }

    #[test]
    fn converts_history() {
        let model = test_model(Api::AnthropicMessages);
        let call = ToolCall {
            id: "toolu_1".into(),
            name: "read".into(),
            arguments: json!({"path": "a"}),
        };
        let mut assistant = AssistantMessage::empty("anthropic-messages", "test", "test-model");
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "plan".into(),
                signature: Some("sig".into()),
                redacted: false,
            }),
            AssistantContent::Text(TextContent {
                text: "Reading.".into(),
            }),
            AssistantContent::ToolCall(call.clone()),
        ];
        let mut result = ToolResultMessage::text(&call, "contents", false);
        result.content.push(UserContent::Image(ImageContent {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        }));
        let context = Context {
            messages: vec![Message::user("read a"), assistant.into(), result.into()],
            ..Context::default()
        };
        let body = build_request(&model, &context, &StreamOptions::default()).body;
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(messages[1]["content"][0]["signature"], "sig");
        assert_eq!(messages[1]["content"][2]["type"], "tool_use");
        assert_eq!(messages[1]["content"][2]["input"]["path"], "a");
        let tool_result = &messages[2]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["tool_use_id"], "toolu_1");
        assert_eq!(tool_result["content"][1]["type"], "image");
        assert_eq!(tool_result["cache_control"]["type"], "ephemeral");
    }

    /// Decodes a recorded SSE transcript without HTTP.
    async fn decode(model: &Model, sse: &str) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
        let model_clone = model.clone();
        let sse = sse.to_owned();
        let skeleton = AssistantMessage::empty("anthropic-messages", &model.provider, &model.id);
        let stream = event_stream(skeleton, move |mut sink| async move {
            let skeleton = sink.message().clone();
            sink.emit(AssistantMessageEvent::Start { message: skeleton });
            let mut decoder = Decoder::new(&model_clone);
            let mut sse_decoder = crate::sse::SseDecoder::new();
            for event in sse_decoder.push(sse.as_bytes()) {
                if let Err(error) = decoder.handle(event.event.as_deref(), &event.data, &mut sink) {
                    return sink.fail(StopReason::Error, error);
                }
            }
            match decoder.finish(&mut sink) {
                Ok(()) => sink.done(),
                Err(error) => sink.fail(StopReason::Error, error),
            }
        });
        let events: Vec<_> = stream.collect().await;
        let message = collect_message(Box::pin(futures::stream::iter(events.clone()))).await;
        (events, message)
    }

    const TOOL_STREAM: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","model":"test-model","usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Need the file."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me "}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"look."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_9","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"src/"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"main.rs\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":50}}

event: message_stop
data: {"type":"message_stop"}

"#;

    #[tokio::test]
    async fn decodes_thinking_text_and_tool_calls() {
        let model = test_model(Api::AnthropicMessages);
        let (events, message) = decode(&model, TOOL_STREAM).await;

        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.response_id.as_deref(), Some("msg_1"));
        assert_eq!(message.text(), "Let me look.");
        let AssistantContent::Thinking(thinking) = &message.content[0] else {
            panic!()
        };
        assert_eq!(thinking.thinking, "Need the file.");
        assert_eq!(thinking.signature.as_deref(), Some("abc"));
        let call = message.tool_calls().next().unwrap();
        assert_eq!(call.id, "toolu_9");
        assert_eq!(call.arguments, json!({"path": "src/main.rs"}));

        assert_eq!(message.usage.input, 100);
        assert_eq!(message.usage.cache_read, 1000);
        assert_eq!(message.usage.output, 50);
        assert_eq!(message.usage.total_tokens, 1150);
        let expected = (100.0 * 3.0 + 50.0 * 15.0 + 1000.0 * 0.3) / 1_000_000.0;
        assert!((message.usage.cost.total - expected).abs() < 1e-12);

        // Folding the events reproduces the final message.
        let mut accumulator = StreamAccumulator::new(AssistantMessage::empty("", "", ""));
        for event in &events[..events.len() - 1] {
            accumulator.apply(event);
        }
        assert_eq!(accumulator.message().content, message.content);
    }

    #[tokio::test]
    async fn reports_stream_errors_and_truncation() {
        let model = test_model(Api::AnthropicMessages);
        let overloaded = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let (_, message) = decode(&model, overloaded).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("Overloaded"));

        let truncated = &TOOL_STREAM[..TOOL_STREAM.find("event: message_delta").unwrap()];
        let (_, message) = decode(&model, truncated).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(
            message
                .error_message
                .as_deref()
                .unwrap()
                .contains("message_stop")
        );
        assert_eq!(message.tool_calls().count(), 1);
    }

    #[tokio::test]
    async fn refusal_is_an_error() {
        let model = test_model(Api::AnthropicMessages);
        let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let (_, message) = decode(&model, sse).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(message.error_message.unwrap().contains("refused"));
    }
}
