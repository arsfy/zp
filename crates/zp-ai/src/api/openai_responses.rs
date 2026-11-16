//! OpenAI Responses API (`POST {base}/responses`, SSE), and its ChatGPT Codex variant
//! (`POST {base}/codex/responses`) used with a ChatGPT sign-in.
//!
//! Reasoning arrives as `reasoning` output items; the whole item (with its encrypted
//! content) is kept as the thinking block's signature and sent back verbatim, since
//! requests use `store: false`. Tool call ids are `call_id|item_id`.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{mapped_level, object_schema, parse_arguments};
use crate::event::{AssistantMessageEvent, AssistantMessageStream, EventSink, event_stream};
use crate::http::{self, HttpError};
use crate::model::{Api, Model, ThinkingLevel};
use crate::options::{DEFAULT_MAX_RETRIES, StreamOptions};
use crate::transform::prepare_messages;
use crate::types::{
    AssistantContent, AssistantMessage, Context, Message, StopReason, ToolCall, UserContent,
};

/// Smallest `max_output_tokens` the API accepts.
const MIN_OUTPUT_TOKENS: u64 = 16;
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Starts a streaming request.
pub(crate) fn stream(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> AssistantMessageStream {
    let skeleton = AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);
    event_stream(skeleton, move |sink| run(model, context, options, sink))
}

/// Whether this is ChatGPT's Codex backend.
fn is_codex(model: &Model) -> bool {
    model.api == Api::OpenAiCodexResponses
}

/// A Sign in with ChatGPT token sent to the OpenAI API (API keys start with `sk-`). Such
/// tokens reject some request fields.
fn is_chatgpt_token(model: &Model, api_key: Option<&str>) -> bool {
    model.provider == "openai"
        && model.base_url.trim_end_matches('/') == OPENAI_BASE_URL
        && api_key.is_some_and(|key| !key.starts_with("sk-"))
}

fn endpoint(model: &Model) -> String {
    let base = model.base_url.trim_end_matches('/');
    if !is_codex(model) {
        return format!("{base}/responses");
    }
    if base.ends_with("/codex/responses") {
        base.to_owned()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}

async fn run(model: Model, context: Context, options: StreamOptions, mut sink: EventSink) {
    let body = build_request(&model, &context, &options);
    let url = endpoint(&model);
    let api_key = options.api_key.clone().unwrap_or_default();
    let bearer = format!("Bearer {api_key}");
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if !api_key.is_empty() {
        headers.push(("authorization", bearer.as_str()));
    }
    let account = is_codex(&model)
        .then(|| crate::oauth::codex_account_id(&api_key))
        .flatten();
    if is_codex(&model) {
        headers.extend([
            ("OpenAI-Beta", "responses=experimental"),
            ("accept", "text/event-stream"),
            ("originator", crate::oauth::CODEX_ORIGINATOR),
        ]);
        if let Some(account) = account.as_deref() {
            headers.push(("chatgpt-account-id", account));
        }
        if let Some(session) = options.session_id.as_deref() {
            headers.push(("session-id", session));
            headers.push(("x-client-request-id", session));
        }
    }
    if model.provider == "github-copilot" {
        headers.extend(super::github_copilot_headers(&context));
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
        &body,
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
        decoder.handle(&event.data, &mut sink)
    })
    .await;
    decoder.close_open(&mut sink);
    if let Err(error) = result {
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
        HttpError::Failed(message) => sink.fail(StopReason::Error, friendly_error(message)),
    }
}

/// Explains ChatGPT usage limits.
fn friendly_error(message: String) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("usage_limit_reached") || lower.contains("usage limit") {
        format!(
            "{message}\nYou have hit your ChatGPT usage limit; see https://chatgpt.com/settings/usage"
        )
    } else {
        message
    }
}

/// Keeps `[A-Za-z0-9_-]`, at most 64 characters, without trailing underscores.
fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    sanitized.trim_end_matches('_').to_owned()
}

/// Ids of calls made by other models: only the call part survives.
fn normalize_foreign_id(id: &str) -> String {
    sanitize_id(id.split('|').next().unwrap_or(id))
}

/// The reasoning effort of the request, or `None` to send no reasoning settings.
fn reasoning(model: &Model, options: &StreamOptions) -> Option<Value> {
    if !model.reasoning {
        return None;
    }
    let level = model.clamp_thinking_level(options.thinking);
    if level != ThinkingLevel::Off {
        return Some(json!({"effort": mapped_level(model, level), "summary": "auto"}));
    }
    if model.provider == "github-copilot" {
        return None;
    }
    // Reasoning models think by default; ask for none where that is possible.
    match model.thinking_level_value(ThinkingLevel::Off) {
        Some(None) => None,
        Some(Some(value)) => Some(json!({"effort": value})),
        None => Some(json!({"effort": "none"})),
    }
}

/// Builds the request body. Pure, for testing.
pub(crate) fn build_request(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let codex = is_codex(model);
    let chatgpt_token = is_chatgpt_token(model, options.api_key.as_deref());
    let mut body = Map::new();
    body.insert("model".into(), Value::from(model.id.clone()));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("store".into(), Value::Bool(false));

    let mut input = Vec::new();
    let system = context.system_prompt.as_deref().filter(|s| !s.is_empty());
    if codex {
        body.insert(
            "instructions".into(),
            Value::from(system.unwrap_or("You are a helpful assistant.")),
        );
    } else if let Some(system) = system {
        let role = if model.reasoning && model.compat.supports_developer_role != Some(false) {
            "developer"
        } else {
            "system"
        };
        input.push(json!({"role": role, "content": system}));
    }
    let messages = prepare_messages(&context.messages, model, normalize_foreign_id);
    input.extend(convert_messages(model, &messages));
    body.insert("input".into(), Value::Array(input));

    if let Some(session) = options.session_id.as_deref() {
        let key: String = session.chars().take(64).collect();
        body.insert("prompt_cache_key".into(), Value::from(key));
    }
    if !codex && !chatgpt_token {
        if let Some(max_tokens) = options.max_tokens {
            body.insert(
                "max_output_tokens".into(),
                Value::from(max_tokens.max(MIN_OUTPUT_TOKENS)),
            );
        }
        if let Some(temperature) = options.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
    }
    if codex {
        body.insert("text".into(), json!({"verbosity": "low"}));
        body.insert("tool_choice".into(), Value::from("auto"));
        body.insert("parallel_tool_calls".into(), Value::Bool(true));
    }

    if !context.tools.is_empty() {
        let strict = model.compat.supports_strict_mode.unwrap_or(false);
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|tool| {
                let mut definition = json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": object_schema(&tool.parameters),
                });
                if strict && !codex {
                    definition["strict"] = Value::Bool(false);
                }
                definition
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    if let Some(reasoning) = reasoning(model, options) {
        let thinking = reasoning.get("summary").is_some();
        body.insert("reasoning".into(), reasoning);
        if thinking || codex || model.provider == "xai" {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    } else if codex {
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    Value::Object(body)
}

fn data_url(mime_type: &str, data: &str) -> String {
    format!("data:{mime_type};base64,{data}")
}

fn user_content(content: &[UserContent]) -> Vec<Value> {
    content
        .iter()
        .map(|block| match block {
            UserContent::Text(text) => json!({"type": "input_text", "text": text.text}),
            UserContent::Image(image) => json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": data_url(&image.mime_type, &image.data),
            }),
        })
        .collect()
}

fn tool_output(model: &Model, content: &[UserContent]) -> Value {
    let text: Vec<&str> = content.iter().filter_map(UserContent::as_text).collect();
    let text = text.join("\n");
    let has_images = content.iter().any(|c| matches!(c, UserContent::Image(_)));
    if !has_images || !model.supports_images() {
        return Value::from(if !text.trim().is_empty() {
            text
        } else if has_images {
            "(see attached image)".to_owned()
        } else {
            "(no tool output)".to_owned()
        });
    }
    let mut blocks = Vec::new();
    if !text.trim().is_empty() {
        blocks.push(json!({"type": "input_text", "text": text}));
    }
    blocks.extend(
        content
            .iter()
            .filter(|c| matches!(c, UserContent::Image(_)))
            .flat_map(|c| user_content(std::slice::from_ref(c))),
    );
    Value::Array(blocks)
}

fn convert_messages(model: &Model, messages: &[Message]) -> Vec<Value> {
    let mut output = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        match message {
            Message::User(user) => {
                let content = user_content(&user.content);
                if !content.is_empty() {
                    output.push(json!({"role": "user", "content": content}));
                }
            }
            Message::Assistant(assistant) => {
                let same_model = assistant.provider == model.provider
                    && assistant.api == model.api.as_str()
                    && assistant.model == model.id;
                let mut text_index = 0;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            // Only reasoning items of this API can be replayed.
                            if let Some(item) = thinking
                                .signature
                                .as_deref()
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                .filter(|item| item["type"] == "reasoning")
                            {
                                output.push(item);
                            }
                        }
                        AssistantContent::Text(text) => {
                            if text.text.is_empty() {
                                continue;
                            }
                            let id = if text_index == 0 {
                                format!("msg_zp_{message_index}")
                            } else {
                                format!("msg_zp_{message_index}_{text_index}")
                            };
                            text_index += 1;
                            output.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text.text, "annotations": []}],
                                "status": "completed",
                                "id": id,
                            }));
                        }
                        AssistantContent::ToolCall(call) => {
                            let (call_id, item_id) = match call.id.split_once('|') {
                                Some((call_id, item_id)) => (call_id, Some(item_id)),
                                None => (call.id.as_str(), None),
                            };
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": call.name,
                                "arguments": call.arguments.to_string(),
                            });
                            if let Some(item_id) =
                                item_id.filter(|id| same_model && id.starts_with("fc_"))
                            {
                                item["id"] = Value::from(item_id);
                            }
                            output.push(item);
                        }
                    }
                }
            }
            Message::ToolResult(result) => {
                let call_id = result
                    .tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or(&result.tool_call_id);
                output.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_output(model, &result.content),
                }));
            }
        }
    }
    output
}

/// An output item being streamed, keyed by `output_index`.
enum Slot {
    Text {
        index: usize,
        streamed: bool,
    },
    Thinking {
        index: usize,
        streamed: bool,
    },
    ToolCall {
        index: usize,
        id: String,
        name: String,
        raw: String,
    },
}

/// Turns Responses API events into ZP events.
pub(crate) struct Decoder<'a> {
    model: &'a Model,
    slots: HashMap<u64, Slot>,
    /// Content index of each reasoning item, to backfill encrypted content.
    reasoning: HashMap<String, usize>,
    stop_reason: Option<StopReason>,
    error: Option<String>,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(model: &'a Model) -> Self {
        Self {
            model,
            slots: HashMap::new(),
            reasoning: HashMap::new(),
            stop_reason: None,
            error: None,
        }
    }

    /// Handles one SSE `data` payload.
    pub(crate) fn handle(&mut self, data: &str, sink: &mut EventSink) -> Result<(), String> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| format!("could not parse Responses event: {error}; data={data}"))?;
        let output_index = event["output_index"].as_u64();
        match event["type"].as_str().unwrap_or_default() {
            "response.created" => {
                if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                    sink.message_mut().response_id = Some(id.to_owned());
                }
            }
            "response.output_item.added" => {
                if let Some(key) = output_index {
                    self.open(key, &event["item"], sink);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(Slot::Thinking { index, streamed }) =
                    output_index.and_then(|key| self.slots.get_mut(&key))
                {
                    *streamed = true;
                    sink.emit(AssistantMessageEvent::ThinkingDelta {
                        index: *index,
                        delta: event["delta"].as_str().unwrap_or_default().to_owned(),
                    });
                }
            }
            "response.reasoning_summary_part.done" => {
                if let Some(Slot::Thinking { index, .. }) =
                    output_index.and_then(|key| self.slots.get(&key))
                {
                    sink.emit(AssistantMessageEvent::ThinkingDelta {
                        index: *index,
                        delta: "\n\n".into(),
                    });
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(Slot::Text { index, streamed }) =
                    output_index.and_then(|key| self.slots.get_mut(&key))
                {
                    *streamed = true;
                    sink.emit(AssistantMessageEvent::TextDelta {
                        index: *index,
                        delta: event["delta"].as_str().unwrap_or_default().to_owned(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(Slot::ToolCall { index, raw, .. }) =
                    output_index.and_then(|key| self.slots.get_mut(&key))
                {
                    let delta = event["delta"].as_str().unwrap_or_default();
                    raw.push_str(delta);
                    sink.emit(AssistantMessageEvent::ToolCallDelta {
                        index: *index,
                        delta: delta.to_owned(),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(Slot::ToolCall { raw, .. }) =
                    output_index.and_then(|key| self.slots.get_mut(&key))
                    && let Some(arguments) = event["arguments"].as_str()
                {
                    arguments.clone_into(raw);
                }
            }
            "response.output_item.done" => {
                if let Some(key) = output_index {
                    if !self.slots.contains_key(&key) {
                        self.open(key, &event["item"], sink);
                    }
                    if let Some(slot) = self.slots.remove(&key) {
                        self.close(slot, Some(&event["item"]), sink);
                    }
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                self.complete(&event["response"], sink);
            }
            "response.failed" => {
                let response = &event["response"];
                let message = match (
                    response.pointer("/error/code").and_then(Value::as_str),
                    response.pointer("/error/message").and_then(Value::as_str),
                ) {
                    (code, Some(message)) => {
                        format!("{}: {message}", code.unwrap_or("error"))
                    }
                    (Some(code), None) => code.to_owned(),
                    (None, None) => response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .map_or_else(
                            || "The response failed".to_owned(),
                            |reason| format!("incomplete: {reason}"),
                        ),
                };
                return Err(friendly_error(message));
            }
            "error" => {
                let code = event["code"]
                    .as_str()
                    .or_else(|| event.pointer("/error/code").and_then(Value::as_str));
                let message = event["message"]
                    .as_str()
                    .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("unknown error");
                return Err(friendly_error(match code {
                    Some(code) => format!("Error {code}: {message}"),
                    None => message.to_owned(),
                }));
            }
            _ => {}
        }
        Ok(())
    }

    fn open(&mut self, key: u64, item: &Value, sink: &mut EventSink) {
        let index = sink.next_index();
        let slot = match item["type"].as_str().unwrap_or_default() {
            "reasoning" => {
                sink.emit(AssistantMessageEvent::ThinkingStart {
                    index,
                    redacted: false,
                });
                Slot::Thinking {
                    index,
                    streamed: false,
                }
            }
            "message" => {
                sink.emit(AssistantMessageEvent::TextStart { index });
                Slot::Text {
                    index,
                    streamed: false,
                }
            }
            "function_call" => {
                let id = format!(
                    "{}|{}",
                    item["call_id"].as_str().unwrap_or_default(),
                    item["id"].as_str().unwrap_or_default()
                );
                let name = item["name"].as_str().unwrap_or_default().to_owned();
                sink.emit(AssistantMessageEvent::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                });
                Slot::ToolCall {
                    index,
                    id,
                    name,
                    raw: item["arguments"].as_str().unwrap_or_default().to_owned(),
                }
            }
            _ => return,
        };
        self.slots.insert(key, slot);
    }

    fn close(&mut self, slot: Slot, item: Option<&Value>, sink: &mut EventSink) {
        match slot {
            Slot::Thinking { index, streamed } => {
                if let Some(item) = item
                    && !streamed
                {
                    let joined = |field: &str| {
                        item[field]
                            .as_array()
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default()
                    };
                    let summary = joined("summary");
                    let text = if summary.is_empty() {
                        joined("content")
                    } else {
                        summary
                    };
                    if !text.is_empty() {
                        sink.emit(AssistantMessageEvent::ThinkingDelta { index, delta: text });
                    }
                }
                if let Some(id) = item.and_then(|item| item["id"].as_str()) {
                    self.reasoning.insert(id.to_owned(), index);
                }
                sink.emit(AssistantMessageEvent::ThinkingEnd {
                    index,
                    signature: item.map(Value::to_string),
                });
            }
            Slot::Text { index, streamed } => {
                if let Some(item) = item
                    && !streamed
                {
                    let text: String = item["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part["text"].as_str().or(part["refusal"].as_str()))
                        .collect();
                    if !text.is_empty() {
                        sink.emit(AssistantMessageEvent::TextDelta { index, delta: text });
                    }
                }
                sink.emit(AssistantMessageEvent::TextEnd { index });
            }
            Slot::ToolCall {
                index,
                id,
                name,
                mut raw,
            } => {
                if let Some(arguments) = item.and_then(|item| item["arguments"].as_str()) {
                    arguments.clone_into(&mut raw);
                }
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

    fn complete(&mut self, response: &Value, sink: &mut EventSink) {
        // Reasoning items sometimes get their encrypted content only in the final output.
        for item in response["output"].as_array().into_iter().flatten() {
            if item["type"] != "reasoning" || item["encrypted_content"].is_null() {
                continue;
            }
            let Some(&index) = item["id"].as_str().and_then(|id| self.reasoning.get(id)) else {
                continue;
            };
            if let Some(AssistantContent::Thinking(thinking)) =
                sink.message_mut().content.get_mut(index)
            {
                let stored = thinking
                    .signature
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok());
                if stored.is_none_or(|stored| stored["encrypted_content"].is_null()) {
                    thinking.signature = Some(item.to_string());
                }
            }
        }
        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
            let read = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
            let cached = read("/input_tokens_details/cached_tokens");
            let cache_write = read("/input_tokens_details/cache_write_tokens");
            let message = sink.message_mut();
            message.usage.input = read("/input_tokens").saturating_sub(cached + cache_write);
            message.usage.output = read("/output_tokens");
            message.usage.cache_read = cached;
            message.usage.cache_write = cache_write;
            message.usage.reasoning = read("/output_tokens_details/reasoning_tokens");
            message.usage.update_total();
            self.model.calculate_cost(&mut message.usage);
        }
        let reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        let (stop_reason, error) = match response["status"].as_str() {
            Some("incomplete") if reason == Some("max_output_tokens") => (StopReason::Length, None),
            Some("incomplete") => (
                StopReason::Error,
                Some(format!(
                    "Response incomplete: {}",
                    reason.unwrap_or("no reason given")
                )),
            ),
            Some("failed" | "cancelled") => (
                StopReason::Error,
                Some(
                    response
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("The response failed")
                        .to_owned(),
                ),
            ),
            _ => (StopReason::Stop, None),
        };
        self.stop_reason = Some(stop_reason);
        self.error = error;
    }

    /// Ends blocks still open when the stream stops.
    pub(crate) fn close_open(&mut self, sink: &mut EventSink) {
        let mut open: Vec<(u64, Slot)> = self.slots.drain().collect();
        open.sort_by_key(|(key, _)| *key);
        for (_, slot) in open {
            self.close(slot, None, sink);
        }
    }

    /// Sets the stop reason once the stream ended.
    pub(crate) fn finish(&mut self, sink: &mut EventSink) -> Result<(), String> {
        let Some(mut stop_reason) = self.stop_reason else {
            return Err("The response stream ended before the response completed".into());
        };
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        let has_tool_calls = sink.message().tool_calls().next().is_some();
        if stop_reason == StopReason::Stop && has_tool_calls {
            stop_reason = StopReason::ToolUse;
        }
        sink.message_mut().stop_reason = stop_reason;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{collect_message, event_stream};
    use crate::model::test_model;
    use crate::types::{TextContent, ThinkingContent, Tool, ToolResultMessage};
    use std::collections::BTreeMap;

    fn model(api: Api, provider: &str) -> Model {
        let mut model = test_model(api);
        model.provider = provider.into();
        model.reasoning = true;
        model.base_url = if api == Api::OpenAiCodexResponses {
            "https://chatgpt.com/backend-api".into()
        } else {
            OPENAI_BASE_URL.into()
        };
        model
    }

    fn context() -> Context {
        Context {
            system_prompt: Some("Be brief.".into()),
            messages: vec![Message::user("hello")],
            tools: vec![Tool {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
        }
    }

    #[test]
    fn endpoints() {
        assert_eq!(
            endpoint(&model(Api::OpenAiResponses, "openai")),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            endpoint(&model(Api::OpenAiCodexResponses, "openai-codex")),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn openai_request() {
        let model = model(Api::OpenAiResponses, "openai");
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            max_tokens: Some(4),
            api_key: Some("sk-test".into()),
            session_id: Some("session".into()),
            ..StreamOptions::default()
        };
        let body = build_request(&model, &context(), &options);
        assert_eq!(body["store"], false);
        assert_eq!(
            body["input"][0],
            json!({"role": "developer", "content": "Be brief."})
        );
        assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["prompt_cache_key"], "session");

        // A ChatGPT sign-in token may not set the output limit.
        let options = StreamOptions {
            api_key: Some("eyJ.token".into()),
            max_tokens: Some(100),
            ..StreamOptions::default()
        };
        let body = build_request(&model, &context(), &options);
        assert!(body.get("max_output_tokens").is_none());
        // Off asks for no reasoning; a model that cannot turn it off reasons anyway.
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
        let mut always = model.clone();
        always.thinking_level_map = Some(BTreeMap::from([(ThinkingLevel::Off, None)]));
        let body = build_request(&always, &context(), &options);
        assert_eq!(body["reasoning"]["summary"], "auto");
        // Copilot sends nothing for off.
        let mut copilot = model.clone();
        copilot.provider = "github-copilot".into();
        assert!(
            build_request(&copilot, &context(), &options)
                .get("reasoning")
                .is_none()
        );
    }

    #[test]
    fn codex_request_uses_instructions() {
        let model = model(Api::OpenAiCodexResponses, "openai-codex");
        let body = build_request(&model, &context(), &StreamOptions::default());
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn history_is_replayed_as_items() {
        let model = model(Api::OpenAiResponses, "openai");
        let reasoning =
            json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
        let mut assistant = AssistantMessage::empty("openai-responses", "openai", &model.id);
        assistant.content = vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "hmm".into(),
                signature: Some(reasoning.to_string()),
                redacted: false,
            }),
            AssistantContent::Text(TextContent {
                text: "Let me look.".into(),
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "call_1|fc_1".into(),
                name: "read".into(),
                arguments: json!({"path": "a"}),
            }),
        ];
        assistant.stop_reason = StopReason::ToolUse;
        let call = assistant.tool_calls().next().unwrap().clone();
        let mut context = context();
        context.messages.push(Message::Assistant(assistant));
        context
            .messages
            .push(Message::ToolResult(ToolResultMessage::text(
                &call, "contents", false,
            )));
        let body = build_request(&model, &context, &StreamOptions::default());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[2], reasoning);
        assert_eq!(input[3]["type"], "message");
        assert_eq!(input[4]["type"], "function_call");
        assert_eq!(input[4]["id"], "fc_1");
        assert_eq!(input[4]["call_id"], "call_1");
        assert_eq!(
            input[5],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "contents"})
        );
    }

    #[tokio::test]
    async fn decodes_a_stream() {
        let model = model(Api::OpenAiResponses, "openai");
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Thinking"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Thinking"}]}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Hi"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "content": [{"type": "output_text", "text": "Hi"}]}}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"path\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"a\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"}}),
            json!({"type": "response.completed", "response": {"status": "completed", "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "enc"}
            ], "usage": {"input_tokens": 100, "output_tokens": 20,
                "input_tokens_details": {"cached_tokens": 60},
                "output_tokens_details": {"reasoning_tokens": 5}}}}),
        ];
        let skeleton = AssistantMessage::empty("openai-responses", "openai", &model.id);
        let message = collect_message(event_stream(skeleton, move |mut sink| async move {
            let mut decoder = Decoder::new(&model);
            for event in events {
                decoder.handle(&event.to_string(), &mut sink).unwrap();
            }
            decoder.close_open(&mut sink);
            decoder.finish(&mut sink).unwrap();
            sink.done();
        }))
        .await;
        assert_eq!(message.response_id.as_deref(), Some("resp_1"));
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.text(), "Hi");
        let AssistantContent::Thinking(thinking) = &message.content[0] else {
            panic!("thinking first")
        };
        assert_eq!(thinking.thinking, "Thinking");
        assert!(thinking.signature.as_deref().unwrap().contains("\"enc\""));
        let call = message.tool_calls().next().unwrap();
        assert_eq!(call.id, "call_1|fc_1");
        assert_eq!(call.arguments, json!({"path": "a"}));
        assert_eq!(
            (
                message.usage.input,
                message.usage.cache_read,
                message.usage.output
            ),
            (40, 60, 20)
        );
    }

    #[tokio::test]
    async fn failures_are_reported() {
        let model = model(Api::OpenAiCodexResponses, "openai-codex");
        let skeleton = AssistantMessage::empty("x", "y", "z");
        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = errors.clone();
        collect_message(event_stream(skeleton, move |mut sink| async move {
            let failed = json!({"type": "response.failed", "response": {"error": {"code": "usage_limit_reached", "message": "Limit"}}});
            let mut decoder = Decoder::new(&model);
            seen.lock().unwrap().push(decoder.handle(&failed.to_string(), &mut sink).unwrap_err());
            // A stream that ends without completing is an error too.
            seen.lock().unwrap().push(Decoder::new(&model).finish(&mut sink).unwrap_err());
            sink.done();
        }))
        .await;
        let errors = errors.lock().unwrap();
        assert!(errors[0].contains("usage limit"), "{}", errors[0]);
        assert!(errors[1].contains("ended before"), "{}", errors[1]);
    }
}
