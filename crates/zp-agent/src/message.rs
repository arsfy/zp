//! Messages in an agent transcript.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zp_ai::{AssistantMessage, Message, ToolResultMessage, UserContent, UserMessage, now_ms};

/// A transcript entry: an LLM message, or an application message that
/// [`crate::AgentHooks::convert_to_llm`] decides how (or whether) to show the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    /// A message the model understands directly.
    Llm(Message),
    /// An application message (`"role": "custom"`).
    Custom(CustomMessage),
}

impl AgentMessage {
    /// Creates a plain-text user message timestamped now.
    pub fn user(text: impl Into<String>) -> Self {
        Self::Llm(Message::user(text))
    }

    /// The LLM message, if this is one.
    pub fn as_llm(&self) -> Option<&Message> {
        match self {
            Self::Llm(message) => Some(message),
            Self::Custom(_) => None,
        }
    }

    /// The assistant message, if this is one.
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Llm(Message::Assistant(message)) => Some(message),
            _ => None,
        }
    }

    /// The tool result, if this is one.
    pub fn as_tool_result(&self) -> Option<&ToolResultMessage> {
        match self {
            Self::Llm(Message::ToolResult(message)) => Some(message),
            _ => None,
        }
    }

    /// Whether this is an assistant message.
    pub fn is_assistant(&self) -> bool {
        self.as_assistant().is_some()
    }

    /// Message timestamp in milliseconds.
    pub fn timestamp(&self) -> i64 {
        match self {
            Self::Llm(message) => message.timestamp(),
            Self::Custom(message) => message.timestamp,
        }
    }
}

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        Self::Llm(message)
    }
}

impl From<UserMessage> for AgentMessage {
    fn from(message: UserMessage) -> Self {
        Self::Llm(message.into())
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(message: AssistantMessage) -> Self {
        Self::Llm(message.into())
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(message: ToolResultMessage) -> Self {
        Self::Llm(message.into())
    }
}

impl From<CustomMessage> for AgentMessage {
    fn from(message: CustomMessage) -> Self {
        Self::Custom(message)
    }
}

/// Marker for the `"role": "custom"` tag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CustomRole {
    /// The only value.
    #[default]
    Custom,
}

/// An application-defined transcript entry (e.g. a compaction summary or a note shown in
/// the UI). Not sent to the model unless a hook converts it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    /// Always `custom`; distinguishes these entries when deserializing.
    pub role: CustomRole,
    /// Application-defined kind, e.g. `compaction_summary`.
    pub custom_type: String,
    /// Content, if it has a textual form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<UserContent>,
    /// Whether the UI should show it.
    #[serde(default)]
    pub display: bool,
    /// Structured data for the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Creation time in milliseconds.
    pub timestamp: i64,
}

impl CustomMessage {
    /// A message of `custom_type` with text content, timestamped now.
    pub fn text(custom_type: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: CustomRole::Custom,
            custom_type: custom_type.into(),
            content: vec![UserContent::text(text)],
            display: true,
            details: None,
            timestamp: now_ms(),
        }
    }
}

/// The default [`crate::AgentHooks::convert_to_llm`]: keeps LLM messages and drops custom
/// ones.
pub fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| message.as_llm().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_flat_and_round_trips() {
        let user = AgentMessage::user("hi");
        let value = serde_json::to_value(&user).unwrap();
        assert_eq!(value["role"], "user");
        assert_eq!(serde_json::from_value::<AgentMessage>(value).unwrap(), user);

        let custom = AgentMessage::from(CustomMessage::text("note", "remember"));
        let value = serde_json::to_value(&custom).unwrap();
        assert_eq!(value["role"], "custom");
        assert_eq!(value["customType"], "note");
        assert_eq!(
            serde_json::from_value::<AgentMessage>(value).unwrap(),
            custom
        );

        assert!(serde_json::from_value::<AgentMessage>(json!({"role": "other"})).is_err());
    }

    #[test]
    fn default_conversion_drops_custom_messages() {
        let messages = vec![
            AgentMessage::user("a"),
            CustomMessage::text("note", "x").into(),
        ];
        assert_eq!(default_convert_to_llm(&messages).len(), 1);
    }
}
