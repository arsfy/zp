//! Conversation types shared by every provider.
//!
//! The JSON shape (camelCase, `role` / `type` tags) is what ZP persists in session files,
//! so changes here must stay backwards compatible.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Everything needed for one model request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    /// System prompt, sent in the provider's native system slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Conversation history, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

/// A tool declaration. `parameters` is a JSON Schema object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// Unique tool name.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments (`{"type": "object", ...}`).
    pub parameters: Value,
}

/// A message in the conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    /// Input from the user.
    User(UserMessage),
    /// A model response.
    Assistant(AssistantMessage),
    /// The result of executing a tool call.
    ToolResult(ToolResultMessage),
}

impl Message {
    /// Creates a plain-text user message timestamped now.
    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserMessage::text(text))
    }

    /// Message timestamp in milliseconds.
    pub fn timestamp(&self) -> i64 {
        match self {
            Self::User(message) => message.timestamp,
            Self::Assistant(message) => message.timestamp,
            Self::ToolResult(message) => message.timestamp,
        }
    }
}

impl From<UserMessage> for Message {
    fn from(message: UserMessage) -> Self {
        Self::User(message)
    }
}

impl From<AssistantMessage> for Message {
    fn from(message: AssistantMessage) -> Self {
        Self::Assistant(message)
    }
}

impl From<ToolResultMessage> for Message {
    fn from(message: ToolResultMessage) -> Self {
        Self::ToolResult(message)
    }
}

/// Content that users and tools can send: text and images.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContent {
    /// Plain text.
    Text(TextContent),
    /// A base64-encoded image.
    Image(ImageContent),
}

impl UserContent {
    /// Creates a text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent { text: text.into() })
    }

    /// Returns the text if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(&text.text),
            Self::Image(_) => None,
        }
    }
}

/// A text block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
    /// The text.
    pub text: String,
}

/// A base64-encoded image block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Base64 data without a `data:` prefix.
    pub data: String,
    /// MIME type, e.g. `image/png`.
    pub mime_type: String,
}

/// A user message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    /// Message content.
    pub content: Vec<UserContent>,
    /// Creation time in milliseconds.
    pub timestamp: i64,
}

impl UserMessage {
    /// Creates a plain-text message timestamped now.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![UserContent::text(text)],
            timestamp: now_ms(),
        }
    }
}

/// Content produced by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    /// Visible answer text.
    Text(TextContent),
    /// Reasoning output.
    Thinking(ThinkingContent),
    /// A request to execute a tool.
    ToolCall(ToolCall),
}

/// A reasoning block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    /// Reasoning text (a summary on providers that do not expose raw reasoning).
    pub thinking: String,
    /// Opaque provider data needed to replay the block (e.g. Anthropic signatures, or the
    /// response field the reasoning arrived in for OpenAI-compatible APIs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Whether the provider redacted the reasoning; `signature` then holds the payload.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

/// A tool call requested by the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Provider-assigned call id, echoed back in the tool result.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Parsed arguments (an object, possibly partial while streaming).
    pub arguments: Value,
}

/// Why the model stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// Natural end of the response.
    #[default]
    Stop,
    /// Hit the output token limit.
    Length,
    /// The model wants tool results.
    ToolUse,
    /// The request failed; see `error_message`.
    Error,
    /// The request was cancelled.
    Aborted,
}

/// A model response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    /// Response content in output order.
    pub content: Vec<AssistantContent>,
    /// Wire protocol that produced the response.
    pub api: String,
    /// Provider id.
    pub provider: String,
    /// Model id that was requested.
    pub model: String,
    /// Token usage and cost.
    pub usage: Usage,
    /// Why the response ended.
    pub stop_reason: StopReason,
    /// Error description when `stop_reason` is `Error` or `Aborted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Provider response id, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Creation time in milliseconds.
    pub timestamp: i64,
}

impl AssistantMessage {
    /// An empty response skeleton for `model`.
    pub fn empty(api: &str, provider: &str, model: &str) -> Self {
        Self {
            content: Vec::new(),
            api: api.to_owned(),
            provider: provider.to_owned(),
            model: model.to_owned(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            response_id: None,
            timestamp: now_ms(),
        }
    }

    /// Concatenated visible text.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Tool calls in output order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

/// The result of a tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    /// Id of the tool call this answers.
    pub tool_call_id: String,
    /// Name of the tool that ran.
    pub tool_name: String,
    /// Result content.
    pub content: Vec<UserContent>,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Creation time in milliseconds.
    pub timestamp: i64,
}

impl ToolResultMessage {
    /// Creates a text result timestamped now.
    pub fn text(call: &ToolCall, text: impl Into<String>, is_error: bool) -> Self {
        Self {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![UserContent::text(text)],
            is_error,
            timestamp: now_ms(),
        }
    }
}

/// Token usage for one response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens (including reasoning).
    pub output: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write: u64,
    /// Reasoning tokens (a subset of `output`), when reported.
    #[serde(default)]
    pub reasoning: u64,
    /// `input + output + cache_read + cache_write`.
    pub total_tokens: u64,
    /// Cost in US dollars.
    pub cost: Cost,
}

impl Usage {
    /// Recomputes `total_tokens` from the components.
    pub fn update_total(&mut self) {
        self.total_tokens = self.input + self.output + self.cache_read + self.cache_write;
    }

    /// All input tokens, cached or not.
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// Cost breakdown in US dollars.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// Uncached input cost.
    pub input: f64,
    /// Output cost.
    pub output: f64,
    /// Cache read cost.
    pub cache_read: f64,
    /// Cache write cost.
    pub cache_write: f64,
    /// Sum of the components.
    pub total: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip_with_tags() {
        let message = Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                    redacted: false,
                }),
                AssistantContent::Text(TextContent { text: "hi".into() }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "a.txt"}),
                }),
            ],
            stop_reason: StopReason::ToolUse,
            ..AssistantMessage::empty("anthropic-messages", "anthropic", "m")
        });
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["stopReason"], "toolUse");
        assert_eq!(value["content"][0]["type"], "thinking");
        assert_eq!(value["content"][2]["type"], "toolCall");
        assert_eq!(value["content"][2]["arguments"]["path"], "a.txt");
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn user_and_tool_result_shapes() {
        let value = serde_json::to_value(Message::user("hello")).unwrap();
        assert_eq!(value["role"], "user");
        assert_eq!(
            value["content"][0],
            json!({"type": "text", "text": "hello"})
        );

        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: json!({}),
        };
        let value =
            serde_json::to_value(Message::from(ToolResultMessage::text(&call, "ok", false)))
                .unwrap();
        assert_eq!(value["role"], "toolResult");
        assert_eq!(value["toolCallId"], "c");
        assert_eq!(value["isError"], false);
    }

    #[test]
    fn assistant_helpers() {
        let mut message = AssistantMessage::empty("a", "p", "m");
        message
            .content
            .push(AssistantContent::Text(TextContent { text: "a".into() }));
        message.content.push(AssistantContent::ToolCall(ToolCall {
            id: "1".into(),
            name: "t".into(),
            arguments: json!({}),
        }));
        message
            .content
            .push(AssistantContent::Text(TextContent { text: "b".into() }));
        assert_eq!(message.text(), "ab");
        assert_eq!(message.tool_calls().count(), 1);
    }
}
