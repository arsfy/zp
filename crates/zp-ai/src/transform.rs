//! Provider-independent cleanup of the conversation before it is sent.

use std::collections::{HashMap, HashSet};

use crate::model::Model;
use crate::types::{
    AssistantContent, Context, Message, StopReason, ToolCall, ToolResultMessage, UserContent,
    now_ms,
};

const USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";

/// Prepares `messages` for `model`:
///
/// - images are replaced by placeholders when the model has no vision support;
/// - reasoning produced by a different model is dropped (signatures only replay on the
///   model that made them) and empty reasoning blocks are removed;
/// - tool call ids from other models are rewritten with `normalize_id` and the matching
///   tool results follow;
/// - failed or aborted responses are skipped, together with results for their calls;
/// - tool calls without a result get a synthetic error result.
pub(crate) fn prepare_messages(
    messages: &[Message],
    model: &Model,
    normalize_id: impl Fn(&str) -> String,
) -> Vec<Message> {
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut result = Vec::with_capacity(messages.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();

    let close_pending =
        |result: &mut Vec<Message>, pending: &mut Vec<ToolCall>, answered: &mut HashSet<String>| {
            for call in pending.drain(..) {
                if !answered.contains(&call.id) {
                    result.push(Message::ToolResult(ToolResultMessage {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        content: vec![UserContent::text("No result provided")],
                        is_error: true,
                        details: None,
                        timestamp: now_ms(),
                    }));
                }
            }
            answered.clear();
        };

    for message in messages {
        match message {
            Message::User(user) => {
                close_pending(&mut result, &mut pending, &mut answered);
                let mut user = user.clone();
                if !model.supports_images() {
                    user.content = replace_images(user.content, USER_IMAGE_PLACEHOLDER);
                }
                result.push(Message::User(user));
            }
            Message::Assistant(assistant) => {
                close_pending(&mut result, &mut pending, &mut answered);
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                let same_model = assistant.provider == model.provider
                    && assistant.api == model.api.as_str()
                    && assistant.model == model.id;
                let mut assistant = assistant.clone();
                assistant.content.retain_mut(|block| match block {
                    AssistantContent::Thinking(thinking) => {
                        same_model
                            && (thinking.redacted
                                || thinking.signature.as_deref().is_some_and(|s| !s.is_empty())
                                || !thinking.thinking.trim().is_empty())
                    }
                    AssistantContent::ToolCall(call) => {
                        if !same_model {
                            let normalized = normalize_id(&call.id);
                            if normalized != call.id {
                                id_map.insert(call.id.clone(), normalized.clone());
                                call.id = normalized;
                            }
                        }
                        true
                    }
                    AssistantContent::Text(_) => true,
                });
                pending = assistant.tool_calls().cloned().collect();
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                let mut tool_result = tool_result.clone();
                if let Some(id) = id_map.get(&tool_result.tool_call_id) {
                    tool_result.tool_call_id.clone_from(id);
                }
                let known = pending
                    .iter()
                    .any(|call| call.id == tool_result.tool_call_id);
                if !known || !answered.insert(tool_result.tool_call_id.clone()) {
                    // Results for skipped or unknown calls (and duplicates) are rejected by
                    // every provider, so they are dropped.
                    continue;
                }
                if !model.supports_images() {
                    tool_result.content =
                        replace_images(tool_result.content, TOOL_IMAGE_PLACEHOLDER);
                }
                result.push(Message::ToolResult(tool_result));
            }
        }
    }
    close_pending(&mut result, &mut pending, &mut answered);
    result
}

/// Replaces image blocks with a placeholder (collapsing consecutive images).
pub(crate) fn replace_images(content: Vec<UserContent>, placeholder: &str) -> Vec<UserContent> {
    let mut output: Vec<UserContent> = Vec::with_capacity(content.len());
    for block in content {
        match block {
            UserContent::Image(_) => {
                if output.last().and_then(UserContent::as_text) != Some(placeholder) {
                    output.push(UserContent::text(placeholder));
                }
            }
            text => output.push(text),
        }
    }
    output
}

/// Rough token estimate (4 characters per token, fixed cost per image).
pub(crate) fn estimate_tokens(context: &Context) -> u64 {
    const IMAGE_TOKENS: u64 = 1200;
    let user_content = |content: &[UserContent]| -> u64 {
        content
            .iter()
            .map(|block| match block {
                UserContent::Text(text) => text.text.len() as u64 / 4,
                UserContent::Image(_) => IMAGE_TOKENS,
            })
            .sum()
    };
    let mut tokens = context
        .system_prompt
        .as_deref()
        .map_or(0, |s| s.len() as u64 / 4);
    for tool in &context.tools {
        tokens += (tool.name.len() + tool.description.len() + tool.parameters.to_string().len())
            as u64
            / 4;
    }
    for message in &context.messages {
        tokens += match message {
            Message::User(user) => user_content(&user.content),
            Message::ToolResult(result) => user_content(&result.content),
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(text) => text.text.len() as u64 / 4,
                    AssistantContent::Thinking(thinking) => thinking.thinking.len() as u64 / 4,
                    AssistantContent::ToolCall(call) => {
                        (call.name.len() + call.arguments.to_string().len()) as u64 / 4
                    }
                })
                .sum(),
        };
    }
    tokens
}

/// Fits an output limit into the context window, leaving a safety margin for estimation
/// error.
pub(crate) fn clamp_max_tokens(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    const SAFETY_TOKENS: u64 = 4096;
    const MIN_OUTPUT_WITH_RATIO_MARGIN: u64 = 32_768;
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let input = estimate_tokens(context);
    let margin = SAFETY_TOKENS.max(input.saturating_mul(15).div_ceil(100));
    let mut room = model.context_window.saturating_sub(input + margin);
    if room < MIN_OUTPUT_WITH_RATIO_MARGIN {
        room = MIN_OUTPUT_WITH_RATIO_MARGIN
            .min(model.context_window.saturating_sub(input + SAFETY_TOKENS));
    }
    max_tokens.min(room.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Api, InputModality, test_model};
    use crate::types::{AssistantMessage, ImageContent, TextContent, ThinkingContent};
    use serde_json::json;

    fn assistant(model: &str, content: Vec<AssistantContent>, stop: StopReason) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            stop_reason: stop,
            ..AssistantMessage::empty("anthropic-messages", "test", model)
        })
    }

    fn call(id: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({}),
        })
    }

    fn result(id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![UserContent::text("ok")],
            is_error: false,
            details: None,
            timestamp: 0,
        })
    }

    #[test]
    fn synthesizes_missing_results_and_drops_orphans() {
        let model = test_model(Api::AnthropicMessages);
        let messages = vec![
            Message::user("hi"),
            assistant(
                "test-model",
                vec![call("a"), call("b")],
                StopReason::ToolUse,
            ),
            result("a"),
            result("zzz"),
            Message::user("next"),
        ];
        let prepared = prepare_messages(&messages, &model, str::to_owned);
        let roles: Vec<_> = prepared
            .iter()
            .map(|m| match m {
                Message::User(_) => "user".to_owned(),
                Message::Assistant(_) => "assistant".to_owned(),
                Message::ToolResult(r) => format!("result:{}:{}", r.tool_call_id, r.is_error),
            })
            .collect();
        assert_eq!(
            roles,
            [
                "user",
                "assistant",
                "result:a:false",
                "result:b:true",
                "user"
            ]
        );
    }

    #[test]
    fn skips_failed_responses_and_their_results() {
        let model = test_model(Api::AnthropicMessages);
        let messages = vec![
            Message::user("hi"),
            assistant("test-model", vec![call("a")], StopReason::Error),
            result("a"),
        ];
        let prepared = prepare_messages(&messages, &model, str::to_owned);
        assert_eq!(prepared.len(), 1);
    }

    #[test]
    fn foreign_thinking_is_dropped_and_ids_normalized() {
        let model = test_model(Api::AnthropicMessages);
        let thinking = AssistantContent::Thinking(ThinkingContent {
            thinking: "secret".into(),
            signature: Some("sig".into()),
            redacted: false,
        });
        let messages = vec![
            assistant(
                "other-model",
                vec![thinking.clone(), call("x|y")],
                StopReason::ToolUse,
            ),
            result("x|y"),
            assistant("test-model", vec![thinking], StopReason::Stop),
        ];
        let prepared = prepare_messages(&messages, &model, |id| id.replace('|', "_"));
        let Message::Assistant(first) = &prepared[0] else {
            panic!()
        };
        assert_eq!(first.content.len(), 1);
        assert_eq!(first.tool_calls().next().unwrap().id, "x_y");
        let Message::ToolResult(tool_result) = &prepared[1] else {
            panic!()
        };
        assert_eq!(tool_result.tool_call_id, "x_y");
        let Message::Assistant(last) = &prepared[2] else {
            panic!()
        };
        assert_eq!(last.content.len(), 1);
    }

    #[test]
    fn images_are_downgraded_for_text_only_models() {
        let mut model = test_model(Api::AnthropicMessages);
        model.input = vec![InputModality::Text];
        let image = UserContent::Image(ImageContent {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        });
        let messages = vec![Message::User(crate::types::UserMessage {
            content: vec![
                image.clone(),
                image,
                UserContent::Text(TextContent { text: "x".into() }),
            ],
            timestamp: 0,
        })];
        let prepared = prepare_messages(&messages, &model, str::to_owned);
        let Message::User(user) = &prepared[0] else {
            panic!()
        };
        assert_eq!(
            user.content,
            vec![
                UserContent::text(USER_IMAGE_PLACEHOLDER),
                UserContent::text("x")
            ]
        );
    }

    #[test]
    fn max_tokens_fit_the_context_window() {
        let mut model = test_model(Api::AnthropicMessages);
        model.context_window = 100_000;
        let context = Context {
            system_prompt: Some("x".repeat(4 * 80_000)),
            ..Context::default()
        };
        // 80k input + 12k margin leaves 8k, below the 32k floor, so only the fixed margin
        // applies: min(32_768, 100_000 - 80_000 - 4_096).
        assert_eq!(clamp_max_tokens(&model, &context, 64_000), 15_904);
        assert_eq!(
            clamp_max_tokens(&model, &Context::default(), 64_000),
            64_000
        );
    }
}
