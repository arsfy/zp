//! OpenAI Chat Completions and compatible APIs (`POST {base}/chat/completions`, SSE).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::{mapped_level, object_schema, parse_arguments};
use crate::event::{AssistantMessageEvent, AssistantMessageStream, EventSink, event_stream};
use crate::http::{self, HttpError};
use crate::model::{MaxTokensField, Model, ThinkingFormat, ThinkingLevel};
use crate::options::{DEFAULT_MAX_RETRIES, StreamOptions};
use crate::transform::{clamp_max_tokens, prepare_messages};
use crate::types::{
    AssistantContent, AssistantMessage, Context, Message, StopReason, ToolCall, UserContent,
};

/// Response fields that carry reasoning text, in priority order. The field a block came
/// from is stored as its signature so it can be replayed in the same field.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

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
    let compat = Compat::resolve(&model);
    let body = build_request(&model, &context, &options, &compat);
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let authorization = options
        .api_key
        .as_deref()
        .map(|key| format!("Bearer {key}"));
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(authorization) = authorization.as_deref() {
        headers.push(("authorization", authorization));
    }
    if compat.send_session_header
        && let Some(session_id) = options.session_id.as_deref()
    {
        headers.push(("x-session-id", session_id));
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
    decoder.close_blocks(&mut sink);
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
        HttpError::Failed(message) => sink.fail(StopReason::Error, message),
    }
}

/// Resolved protocol quirks: explicit model settings over provider auto-detection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Compat {
    pub(crate) supports_store: bool,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_reasoning_effort: bool,
    pub(crate) supports_usage_in_streaming: bool,
    pub(crate) max_tokens_field: MaxTokensField,
    pub(crate) requires_tool_result_name: bool,
    pub(crate) requires_assistant_after_tool_result: bool,
    pub(crate) requires_thinking_as_text: bool,
    pub(crate) requires_reasoning_content: bool,
    pub(crate) thinking_format: ThinkingFormat,
    pub(crate) supports_strict_mode: bool,
    pub(crate) zai_tool_stream: bool,
    pub(crate) anthropic_cache_control: bool,
    pub(crate) send_session_header: bool,
}

impl Compat {
    pub(crate) fn resolve(model: &Model) -> Self {
        let provider = model.provider.as_str();
        let base_url = model.base_url.to_ascii_lowercase();
        let is_zai = provider.starts_with("zai")
            || base_url.contains("api.z.ai")
            || base_url.contains("open.bigmodel.cn");
        let is_moonshot = provider.starts_with("moonshotai") || base_url.contains("api.moonshot.");
        let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
        let is_deepseek = provider == "deepseek" || base_url.contains("deepseek.com");
        let is_together = provider == "together" || base_url.contains("api.together.");
        let is_cerebras = provider == "cerebras" || base_url.contains("cerebras.ai");
        let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
        let is_grok = provider == "xai" || base_url.contains("api.x.ai");
        let non_standard = is_zai
            || is_moonshot
            || is_deepseek
            || is_together
            || is_cerebras
            || is_nvidia
            || is_grok
            || base_url.contains("chutes.ai");
        let use_max_tokens = is_deepseek
            || is_moonshot
            || is_zai
            || is_together
            || is_nvidia
            || base_url.contains("chutes.ai");
        let openrouter_developer_role = is_openrouter
            && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));

        let compat = &model.compat;
        Self {
            supports_store: compat.supports_store.unwrap_or(!non_standard),
            supports_developer_role: compat
                .supports_developer_role
                .unwrap_or(openrouter_developer_role || (!non_standard && !is_openrouter)),
            supports_reasoning_effort: compat
                .supports_reasoning_effort
                .unwrap_or(!is_grok && !is_zai && !is_moonshot && !is_together && !is_nvidia),
            supports_usage_in_streaming: compat.supports_usage_in_streaming.unwrap_or(true),
            max_tokens_field: compat.max_tokens_field.unwrap_or(if use_max_tokens {
                MaxTokensField::MaxTokens
            } else {
                MaxTokensField::MaxCompletionTokens
            }),
            requires_tool_result_name: compat.requires_tool_result_name.unwrap_or(false),
            requires_assistant_after_tool_result: compat
                .requires_assistant_after_tool_result
                .unwrap_or(false),
            requires_thinking_as_text: compat.requires_thinking_as_text.unwrap_or(false),
            requires_reasoning_content: compat
                .requires_reasoning_content_on_assistant_messages
                .unwrap_or(is_deepseek),
            thinking_format: compat.thinking_format.unwrap_or(if is_deepseek {
                ThinkingFormat::Deepseek
            } else if is_zai {
                ThinkingFormat::Zai
            } else if is_together {
                ThinkingFormat::Together
            } else if is_openrouter {
                ThinkingFormat::Openrouter
            } else {
                ThinkingFormat::Openai
            }),
            supports_strict_mode: compat.supports_strict_mode.unwrap_or(false),
            zai_tool_stream: compat.zai_tool_stream.unwrap_or(false),
            anthropic_cache_control: compat
                .cache_control_format
                .as_deref()
                .map(|format| format == "anthropic")
                .unwrap_or(is_openrouter && model.id.starts_with("anthropic/")),
            send_session_header: is_openrouter,
        }
    }
}

/// Builds the request body. Pure, for testing.
pub(crate) fn build_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &Compat,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::from(model.id.clone()));
    body.insert("stream".into(), Value::Bool(true));
    if compat.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        body.insert("store".into(), Value::Bool(false));
    }

    let max_tokens = clamp_max_tokens(
        model,
        context,
        options.max_tokens.unwrap_or(model.max_tokens).max(1),
    );
    let field = match compat.max_tokens_field {
        MaxTokensField::MaxTokens => "max_tokens",
        MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
    };
    body.insert(field.into(), Value::from(max_tokens));

    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }

    let mut messages = convert_messages(model, context, compat);
    let mut tools = convert_tools(context, compat);
    if compat.anthropic_cache_control {
        apply_anthropic_cache_control(&mut messages, &mut tools);
    }
    body.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if compat.zai_tool_stream {
            body.insert("tool_stream".into(), Value::Bool(true));
        }
    }

    apply_reasoning(&mut body, model, options.thinking, compat);
    Value::Object(body)
}

fn apply_reasoning(
    body: &mut Map<String, Value>,
    model: &Model,
    requested: ThinkingLevel,
    compat: &Compat,
) {
    if !model.reasoning {
        return;
    }
    let level = model.clamp_thinking_level(requested);
    let effort = (level != ThinkingLevel::Off).then(|| mapped_level(model, level));
    // `off` is configurable unless the model maps it to null (always reasoning).
    let off_value = model.thinking_level_value(ThinkingLevel::Off);
    let can_disable = off_value != Some(None);
    match compat.thinking_format {
        ThinkingFormat::Deepseek => {
            if effort.is_some() {
                body.insert("thinking".into(), json!({"type": "enabled"}));
            } else if can_disable {
                body.insert("thinking".into(), json!({"type": "disabled"}));
            }
            if let Some(effort) = effort.filter(|_| compat.supports_reasoning_effort) {
                body.insert("reasoning_effort".into(), Value::from(effort));
            }
        }
        ThinkingFormat::Zai => {
            body.insert(
                "thinking".into(),
                if effort.is_some() {
                    json!({"type": "enabled", "clear_thinking": false})
                } else {
                    json!({"type": "disabled"})
                },
            );
            if let Some(effort) = effort.filter(|_| compat.supports_reasoning_effort) {
                body.insert("reasoning_effort".into(), Value::from(effort));
            }
        }
        ThinkingFormat::Openrouter => {
            if let Some(effort) = effort {
                body.insert("reasoning".into(), json!({"effort": effort}));
            } else if can_disable {
                let off = off_value.flatten().unwrap_or("none");
                body.insert("reasoning".into(), json!({"effort": off}));
            }
        }
        ThinkingFormat::Qwen => {
            body.insert("enable_thinking".into(), Value::Bool(effort.is_some()));
            if let Some(effort) = effort.filter(|_| compat.supports_reasoning_effort) {
                body.insert("reasoning_effort".into(), Value::from(effort));
            }
        }
        ThinkingFormat::Together => {
            body.insert("reasoning".into(), json!({"enabled": effort.is_some()}));
            if let Some(effort) = effort.filter(|_| compat.supports_reasoning_effort) {
                body.insert("reasoning_effort".into(), Value::from(effort));
            }
        }
        ThinkingFormat::Openai => {
            if !compat.supports_reasoning_effort {
                return;
            }
            if let Some(effort) = effort {
                body.insert("reasoning_effort".into(), Value::from(effort));
            } else if let Some(Some(off)) = off_value {
                body.insert("reasoning_effort".into(), Value::from(off));
            }
        }
        ThinkingFormat::Other => {}
    }
}

fn convert_tools(context: &Context, compat: &Compat) -> Vec<Value> {
    context
        .tools
        .iter()
        .map(|tool| {
            let mut function = json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": object_schema(&tool.parameters),
            });
            if compat.supports_strict_mode {
                function["strict"] = Value::Bool(false);
            }
            json!({"type": "function", "function": function})
        })
        .collect()
}

fn image_part(data: &str, mime_type: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}})
}

fn convert_messages(model: &Model, context: &Context, compat: &Compat) -> Vec<Value> {
    let mut output = Vec::new();
    if let Some(system) = context.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        output.push(json!({"role": role, "content": system}));
    }

    let messages = prepare_messages(&context.messages, model, |id| {
        if model.provider == "openai" {
            id.chars().take(40).collect()
        } else {
            id.to_owned()
        }
    });
    let bridge = || json!({"role": "assistant", "content": "I have processed the tool results."});
    let mut last_was_tool_result = false;
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(user) => {
                if compat.requires_assistant_after_tool_result && last_was_tool_result {
                    output.push(bridge());
                }
                let parts: Vec<Value> = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserContent::Text(text) if text.text.is_empty() => None,
                        UserContent::Text(text) => Some(json!({"type": "text", "text": text.text})),
                        UserContent::Image(image) => {
                            Some(image_part(&image.data, &image.mime_type))
                        }
                    })
                    .collect();
                if !parts.is_empty() {
                    let content = match parts.as_slice() {
                        [single] if single["type"] == "text" => single["text"].clone(),
                        _ => Value::Array(parts),
                    };
                    output.push(json!({"role": "user", "content": content}));
                }
                last_was_tool_result = false;
            }
            Message::Assistant(assistant) => {
                if let Some(message) = convert_assistant(model, assistant, compat) {
                    output.push(message);
                }
                last_was_tool_result = false;
            }
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    let text: Vec<&str> = result
                        .content
                        .iter()
                        .filter_map(UserContent::as_text)
                        .collect();
                    let text = text.join("\n");
                    let has_images = result
                        .content
                        .iter()
                        .any(|b| matches!(b, UserContent::Image(_)));
                    let content = if !text.trim().is_empty() {
                        text
                    } else if has_images {
                        "(see attached image)".to_owned()
                    } else {
                        "(no tool output)".to_owned()
                    };
                    let mut message = json!({
                        "role": "tool",
                        "content": content,
                        "tool_call_id": result.tool_call_id,
                    });
                    if compat.requires_tool_result_name {
                        message["name"] = Value::from(result.tool_name.clone());
                    }
                    output.push(message);
                    if model.supports_images() {
                        images.extend(result.content.iter().filter_map(|block| match block {
                            UserContent::Image(image) => {
                                Some(image_part(&image.data, &image.mime_type))
                            }
                            UserContent::Text(_) => None,
                        }));
                    }
                    index += 1;
                }
                if images.is_empty() {
                    last_was_tool_result = true;
                } else {
                    // Tool messages cannot carry images; attach them in a user message.
                    if compat.requires_assistant_after_tool_result {
                        output.push(bridge());
                    }
                    let mut parts = vec![
                        json!({"type": "text", "text": "Attached image(s) from tool result:"}),
                    ];
                    parts.extend(images);
                    output.push(json!({"role": "user", "content": parts}));
                    last_was_tool_result = false;
                }
                continue;
            }
        }
        index += 1;
    }
    output
}

fn convert_assistant(
    model: &Model,
    assistant: &AssistantMessage,
    compat: &Compat,
) -> Option<Value> {
    let text: String = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                Some(text.text.as_str())
            }
            _ => None,
        })
        .collect();
    let thinking: Vec<_> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Thinking(thinking) if !thinking.thinking.trim().is_empty() => {
                Some(thinking)
            }
            _ => None,
        })
        .collect();
    let tool_calls: Vec<Value> = assistant
        .tool_calls()
        .map(|call| {
            json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments.to_string()},
            })
        })
        .collect();

    let mut message = Map::new();
    message.insert("role".into(), Value::from("assistant"));
    let mut content = if text.is_empty() {
        Value::Null
    } else {
        Value::from(text.clone())
    };
    if !thinking.is_empty() {
        let joined = thinking
            .iter()
            .map(|t| t.thinking.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if compat.requires_thinking_as_text {
            content = Value::from(if text.is_empty() {
                joined
            } else {
                format!("{joined}\n\n{text}")
            });
        } else if let Some(field) = thinking[0]
            .signature
            .as_deref()
            .filter(|field| REASONING_FIELDS.contains(field))
        {
            message.insert(field.into(), Value::from(joined));
        }
    }
    if compat.requires_reasoning_content
        && model.reasoning
        && !message.contains_key("reasoning_content")
    {
        message.insert("reasoning_content".into(), Value::from(""));
    }
    let has_content = content.as_str().is_some_and(|s| !s.is_empty());
    if !has_content && tool_calls.is_empty() {
        return None;
    }
    if content.is_null() && compat.requires_assistant_after_tool_result {
        content = Value::from("");
    }
    message.insert("content".into(), content);
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Some(Value::Object(message))
}

/// Claude behind OpenRouter needs explicit cache breakpoints.
fn apply_anthropic_cache_control(messages: &mut [Value], tools: &mut [Value]) {
    let cache_control = json!({"type": "ephemeral"});
    let mark = |message: &mut Value| match message.get_mut("content") {
        Some(Value::String(text)) => {
            let text = std::mem::take(text);
            message["content"] =
                json!([{"type": "text", "text": text, "cache_control": cache_control}]);
        }
        Some(Value::Array(parts)) => {
            if let Some(part) = parts.iter_mut().rev().find(|part| part["type"] == "text") {
                part["cache_control"] = cache_control.clone();
            }
        }
        _ => {}
    };
    if let Some(system) = messages
        .iter_mut()
        .find(|m| m["role"] == "system" || m["role"] == "developer")
    {
        mark(system);
    }
    if let Some(last) = messages
        .iter_mut()
        .rev()
        .find(|m| m["role"] == "user" || m["role"] == "tool")
        .filter(|m| m["role"] == "user")
    {
        mark(last);
    }
    if let Some(tool) = tools.last_mut() {
        tool["function"]["cache_control"] = cache_control.clone();
    }
}

enum Block {
    Text,
    Thinking {
        field: String,
    },
    ToolCall {
        id: String,
        name: String,
        raw: String,
    },
}

/// Turns Chat Completions chunks into ZP events.
pub(crate) struct Decoder<'a> {
    model: &'a Model,
    /// Open blocks by content index, in creation order.
    blocks: Vec<(usize, Block)>,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    tool_by_stream_index: HashMap<u64, usize>,
    tool_by_id: HashMap<String, usize>,
    stop_reason: Option<StopReason>,
    error: Option<String>,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(model: &'a Model) -> Self {
        Self {
            model,
            blocks: Vec::new(),
            text_index: None,
            thinking_index: None,
            tool_by_stream_index: HashMap::new(),
            tool_by_id: HashMap::new(),
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
        let chunk: Value = serde_json::from_str(data)
            .map_err(|error| format!("could not parse stream chunk: {error}; data={data}"))?;
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string());
            return Err(message);
        }
        if sink.message().response_id.is_none()
            && let Some(id) = chunk["id"].as_str()
        {
            sink.message_mut().response_id = Some(id.to_owned());
        }
        let choice = chunk["choices"].get(0);
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.update_usage(usage, sink);
        } else if let Some(usage) = choice
            .and_then(|c| c.get("usage"))
            .filter(|u| u.is_object())
        {
            self.update_usage(usage, sink);
        }
        let Some(choice) = choice else {
            return Ok(());
        };
        if let Some(reason) = choice["finish_reason"].as_str() {
            let (stop_reason, error) = map_finish_reason(reason);
            self.stop_reason = Some(stop_reason);
            self.error = error;
        }
        let delta = &choice["delta"];
        if !delta.is_object() {
            return Ok(());
        }
        // Reasoning first: some proxies send reasoning and answer in the same delta.
        if let Some((field, text)) = REASONING_FIELDS.iter().find_map(|field| {
            delta[*field]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(|t| (*field, t))
        }) {
            let index = match self.thinking_index {
                Some(index) => index,
                None => {
                    let index = sink.next_index();
                    sink.emit(AssistantMessageEvent::ThinkingStart {
                        index,
                        redacted: false,
                    });
                    self.blocks.push((
                        index,
                        Block::Thinking {
                            field: field.to_owned(),
                        },
                    ));
                    self.thinking_index = Some(index);
                    index
                }
            };
            sink.emit(AssistantMessageEvent::ThinkingDelta {
                index,
                delta: text.to_owned(),
            });
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            let index = match self.text_index {
                Some(index) => index,
                None => {
                    let index = sink.next_index();
                    sink.emit(AssistantMessageEvent::TextStart { index });
                    self.blocks.push((index, Block::Text));
                    self.text_index = Some(index);
                    index
                }
            };
            sink.emit(AssistantMessageEvent::TextDelta {
                index,
                delta: text.to_owned(),
            });
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                self.tool_call_delta(call, sink);
            }
        }
        Ok(())
    }

    fn tool_call_delta(&mut self, call: &Value, sink: &mut EventSink) {
        let stream_index = call["index"].as_u64();
        let id = call["id"].as_str().filter(|id| !id.is_empty());
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty());
        let existing = stream_index
            .and_then(|i| self.tool_by_stream_index.get(&i))
            .or_else(|| id.and_then(|id| self.tool_by_id.get(id)))
            .copied();
        let index = match existing {
            Some(index) => index,
            None => {
                let index = sink.next_index();
                sink.emit(AssistantMessageEvent::ToolCallStart {
                    index,
                    id: id.unwrap_or_default().to_owned(),
                    name: name.unwrap_or_default().to_owned(),
                });
                self.blocks.push((
                    index,
                    Block::ToolCall {
                        id: id.unwrap_or_default().to_owned(),
                        name: name.unwrap_or_default().to_owned(),
                        raw: String::new(),
                    },
                ));
                index
            }
        };
        if let Some(stream_index) = stream_index {
            self.tool_by_stream_index.insert(stream_index, index);
        }
        if let Some(id) = id {
            self.tool_by_id.insert(id.to_owned(), index);
        }
        let Some((
            _,
            Block::ToolCall {
                id: block_id,
                name: block_name,
                raw,
            },
        )) = self.blocks.iter_mut().find(|(i, _)| *i == index)
        else {
            return;
        };
        if block_id.is_empty()
            && let Some(id) = id
        {
            id.clone_into(block_id);
        }
        if block_name.is_empty()
            && let Some(name) = name
        {
            name.clone_into(block_name);
        }
        // Keep the partial message's id and name in sync once they arrive.
        if let Some(AssistantContent::ToolCall(partial)) = sink.message_mut().content.get_mut(index)
        {
            partial.id.clone_from(block_id);
            partial.name.clone_from(block_name);
        }
        if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
            && !arguments.is_empty()
        {
            raw.push_str(arguments);
            sink.emit(AssistantMessageEvent::ToolCallDelta {
                index,
                delta: arguments.to_owned(),
            });
        }
    }

    fn update_usage(&self, usage: &Value, sink: &mut EventSink) {
        let read = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
        let prompt = read("/prompt_tokens").unwrap_or(0);
        let cache_read = read("/prompt_tokens_details/cached_tokens")
            .or_else(|| read("/prompt_cache_hit_tokens"))
            .or_else(|| read("/cached_tokens"))
            .unwrap_or(0);
        let cache_write = read("/prompt_tokens_details/cache_write_tokens").unwrap_or(0);
        let message = sink.message_mut();
        message.usage.input = prompt.saturating_sub(cache_read + cache_write);
        message.usage.output = read("/completion_tokens").unwrap_or(0);
        message.usage.cache_read = cache_read;
        message.usage.cache_write = cache_write;
        message.usage.reasoning = read("/completion_tokens_details/reasoning_tokens").unwrap_or(0);
        message.usage.update_total();
        self.model.calculate_cost(&mut message.usage);
        // OpenRouter reports the billed cost directly.
        if let Some(cost) = usage.get("cost").and_then(Value::as_f64) {
            message.usage.cost.total = cost;
        }
        sink.emit_usage();
    }

    /// Ends all open blocks in content order.
    pub(crate) fn close_blocks(&mut self, sink: &mut EventSink) {
        for (index, block) in self.blocks.drain(..) {
            match block {
                Block::Text => {
                    sink.emit(AssistantMessageEvent::TextEnd { index });
                }
                Block::Thinking { field } => {
                    sink.emit(AssistantMessageEvent::ThinkingEnd {
                        index,
                        signature: Some(field),
                    });
                }
                Block::ToolCall { id, name, raw } => {
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
        self.text_index = None;
        self.thinking_index = None;
    }

    /// Validates the end of the stream and records the stop reason.
    pub(crate) fn finish(&mut self, sink: &mut EventSink) -> Result<(), String> {
        self.close_blocks(sink);
        let Some(stop_reason) = self.stop_reason else {
            return Err("Stream ended without finish_reason".into());
        };
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        sink.message_mut().stop_reason = stop_reason;
        Ok(())
    }
}

fn map_finish_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "tool_calls" | "function_call" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::collect_message;
    use crate::model::{Api, test_model};
    use crate::types::{TextContent, ThinkingContent, Tool, ToolResultMessage};
    use futures::StreamExt as _;
    use std::collections::BTreeMap;

    fn model(provider: &str, base_url: &str) -> Model {
        let mut model = test_model(Api::OpenAiCompletions);
        model.provider = provider.into();
        model.base_url = base_url.into();
        model
    }

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
    fn compat_detection() {
        let deepseek = Compat::resolve(&model("deepseek", "https://api.deepseek.com"));
        assert!(!deepseek.supports_store);
        assert!(!deepseek.supports_developer_role);
        assert_eq!(deepseek.max_tokens_field, MaxTokensField::MaxTokens);
        assert_eq!(deepseek.thinking_format, ThinkingFormat::Deepseek);
        assert!(deepseek.requires_reasoning_content);

        let mut openrouter_claude = model("openrouter", "https://openrouter.ai/api/v1");
        openrouter_claude.id = "anthropic/claude-x".into();
        let compat = Compat::resolve(&openrouter_claude);
        assert!(compat.anthropic_cache_control);
        assert!(compat.supports_developer_role);
        assert_eq!(compat.thinking_format, ThinkingFormat::Openrouter);

        let generic = Compat::resolve(&model("local", "http://localhost:11434/v1"));
        assert!(generic.supports_store);
        assert_eq!(
            generic.max_tokens_field,
            MaxTokensField::MaxCompletionTokens
        );

        let mut explicit = model("zai", "https://api.z.ai/api/coding/paas/v4");
        explicit.compat.supports_store = Some(true);
        let compat = Compat::resolve(&explicit);
        assert!(compat.supports_store);
        assert_eq!(compat.thinking_format, ThinkingFormat::Zai);
    }

    #[test]
    fn builds_request() {
        let mut model = model("local", "http://localhost/v1");
        model.reasoning = true;
        let compat = Compat::resolve(&model);
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            ..StreamOptions::default()
        };
        let body = build_request(&model, &context(), &options, &compat);
        assert_eq!(
            body["messages"][0],
            json!({"role": "developer", "content": "Be brief."})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "hello"})
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["max_completion_tokens"], 64_000);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert!(body["tools"][0]["function"].get("strict").is_none());
    }

    #[test]
    fn thinking_formats() {
        let mut deepseek = model("deepseek", "https://api.deepseek.com");
        deepseek.reasoning = true;
        deepseek.thinking_level_map = Some(BTreeMap::from([
            (ThinkingLevel::Minimal, None),
            (ThinkingLevel::Low, None),
            (ThinkingLevel::Medium, None),
            (ThinkingLevel::Max, Some("max".into())),
        ]));
        let compat = Compat::resolve(&deepseek);
        let body = build_request(&deepseek, &context(), &StreamOptions::default(), &compat);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
        let options = StreamOptions {
            thinking: ThinkingLevel::Low,
            ..StreamOptions::default()
        };
        let body = build_request(&deepseek, &context(), &options, &compat);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");

        let mut zai = model("zai", "https://api.z.ai/api/coding/paas/v4");
        zai.reasoning = true;
        zai.compat.zai_tool_stream = Some(true);
        let compat = Compat::resolve(&zai);
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            ..StreamOptions::default()
        };
        let body = build_request(&zai, &context(), &options, &compat);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "clear_thinking": false})
        );
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["tool_stream"], true);
        assert_eq!(body["max_tokens"], 64_000);

        let mut openrouter = model("openrouter", "https://openrouter.ai/api/v1");
        openrouter.reasoning = true;
        openrouter.thinking_level_map =
            Some(BTreeMap::from([(ThinkingLevel::Off, Some("none".into()))]));
        let compat = Compat::resolve(&openrouter);
        let body = build_request(&openrouter, &context(), &StreamOptions::default(), &compat);
        assert_eq!(body["reasoning"]["effort"], "none");
    }

    #[test]
    fn converts_history_with_reasoning_and_tools() {
        let mut model = model("deepseek", "https://api.deepseek.com");
        model.reasoning = true;
        let compat = Compat::resolve(&model);
        let call = ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({"path": "a"}),
        };
        let mut assistant = AssistantMessage::empty("openai-completions", "deepseek", "test-model");
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "plan".into(),
                signature: Some("reasoning_content".into()),
                redacted: false,
            }),
            AssistantContent::Text(TextContent {
                text: "Reading.".into(),
            }),
            AssistantContent::ToolCall(call.clone()),
        ];
        let context = Context {
            messages: vec![
                Message::user("read a"),
                assistant.into(),
                ToolResultMessage::text(&call, "contents", false).into(),
            ],
            ..Context::default()
        };
        let body = build_request(&model, &context, &StreamOptions::default(), &compat);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["reasoning_content"], "plan");
        assert_eq!(messages[1]["content"], "Reading.");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a"}"#
        );
        assert_eq!(
            messages[2],
            json!({"role": "tool", "content": "contents", "tool_call_id": "call_1"})
        );
    }

    async fn decode(model: &Model, chunks: &[&str]) -> AssistantMessage {
        let model_clone = model.clone();
        let chunks: Vec<String> = chunks.iter().map(|c| (*c).to_owned()).collect();
        let skeleton = AssistantMessage::empty("openai-completions", &model.provider, &model.id);
        let stream = event_stream(skeleton, move |mut sink| async move {
            let skeleton = sink.message().clone();
            sink.emit(AssistantMessageEvent::Start { message: skeleton });
            let mut decoder = Decoder::new(&model_clone);
            for chunk in chunks {
                if let Err(error) = decoder.handle(&chunk, &mut sink) {
                    decoder.close_blocks(&mut sink);
                    return sink.fail(StopReason::Error, error);
                }
            }
            match decoder.finish(&mut sink) {
                Ok(()) => sink.done(),
                Err(error) => sink.fail(StopReason::Error, error),
            }
        });
        let events: Vec<_> = stream.collect().await;
        collect_message(Box::pin(futures::stream::iter(events))).await
    }

    #[tokio::test]
    async fn decodes_reasoning_text_and_split_tool_calls() {
        let model = model("deepseek", "https://api.deepseek.com");
        let message = decode(&model, &[
            r#"{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Think"}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"ing."}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"Sure."}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"ls","arguments":"{}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,"prompt_cache_hit_tokens":600,"completion_tokens_details":{"reasoning_tokens":5}}}"#,
            "[DONE]",
        ])
        .await;
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.response_id.as_deref(), Some("c1"));
        let AssistantContent::Thinking(thinking) = &message.content[0] else {
            panic!()
        };
        assert_eq!(thinking.thinking, "Thinking.");
        assert_eq!(thinking.signature.as_deref(), Some("reasoning_content"));
        assert_eq!(message.text(), "Sure.");
        let calls: Vec<_> = message.tool_calls().collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].id.as_str(), calls[0].name.as_str()),
            ("call_a", "read")
        );
        assert_eq!(calls[0].arguments, json!({"path": "a.rs"}));
        assert_eq!(calls[1].arguments, json!({}));
        assert_eq!(message.usage.input, 400);
        assert_eq!(message.usage.cache_read, 600);
        assert_eq!(message.usage.output, 20);
        assert_eq!(message.usage.reasoning, 5);
        assert_eq!(message.usage.total_tokens, 1020);
    }

    #[tokio::test]
    async fn errors_and_missing_finish_reason() {
        let model = model("local", "http://localhost/v1");
        let message = decode(
            &model,
            &[
                r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
                r#"{"error":{"message":"rate limited","code":429}}"#,
            ],
        )
        .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("rate limited"));
        assert_eq!(message.text(), "partial");

        let message = decode(&model, &[r#"{"choices":[{"delta":{"content":"x"}}]}"#]).await;
        assert_eq!(message.stop_reason, StopReason::Error);

        let message = decode(
            &model,
            &[r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#],
        )
        .await;
        assert_eq!(
            message.error_message.as_deref(),
            Some("Provider finish_reason: content_filter")
        );
    }
}
