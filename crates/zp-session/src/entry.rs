//! The on-disk records of a session file.
//!
//! A session file is JSON Lines: one [`SessionHeader`] followed by [`SessionEntry`]s. Every
//! entry names its parent, so the file is an append-only tree; the active branch is the
//! path from the root to the current leaf.
//!
//! ```text
//! {"type":"session","version":1,"id":"0196…","timestamp":1763193801123,"cwd":"/work/app"}
//! {"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":…,"message":{"role":"user",…}}
//! {"type":"model_change","id":"…","parentId":"a1b2c3d4","timestamp":…,"provider":"anthropic","modelId":"…"}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zp_agent::AgentMessage;
use zp_ai::ThinkingLevel;

/// Current file format version.
pub const SESSION_VERSION: u32 = 1;

/// Marker for the header's `"type": "session"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderType {
    /// The only value.
    #[default]
    Session,
}

/// First line of a session file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// Always `session`.
    #[serde(rename = "type")]
    pub kind: HeaderType,
    /// File format version.
    pub version: u32,
    /// Session id (UUID v7).
    pub id: String,
    /// Creation time (Unix milliseconds).
    pub timestamp: i64,
    /// Project directory the session belongs to.
    pub cwd: String,
    /// The session file this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// One node of the session tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    /// Short id, unique within the session.
    pub id: String,
    /// Parent entry; `None` for a root.
    pub parent_id: Option<String>,
    /// Creation time (Unix milliseconds).
    pub timestamp: i64,
    /// What the entry records.
    #[serde(flatten)]
    pub data: EntryData,
}

/// Entry payloads, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EntryData {
    /// A transcript message.
    Message {
        /// The message.
        message: AgentMessage,
        /// Active run time in whole seconds when an assistant response finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_seconds: Option<u64>,
    },
    /// The model was switched.
    ModelChange {
        /// Provider id.
        provider: String,
        /// Model id.
        model_id: String,
    },
    /// The reasoning level was changed.
    ThinkingLevelChange {
        /// New level.
        thinking_level: ThinkingLevel,
    },
    /// Session metadata, e.g. its display name.
    SessionInfo {
        /// Display name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Sets (or with `None` clears) a bookmark label on an entry.
    Label {
        /// Labeled entry.
        target_id: String,
        /// Label text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Older context replaced by a summary.
    Compaction {
        /// Summary of the replaced messages.
        summary: String,
        /// First entry kept verbatim after the summary.
        first_kept_entry_id: String,
        /// Context size before compaction.
        tokens_before: u64,
        /// Implementation-specific data.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
}

impl SessionEntry {
    /// The message, if this is a message entry.
    pub fn message(&self) -> Option<&AgentMessage> {
        match &self.data {
            EntryData::Message { message, .. } => Some(message),
            _ => None,
        }
    }

    /// The `type` tag.
    pub fn kind(&self) -> &'static str {
        match self.data {
            EntryData::Message { .. } => "message",
            EntryData::ModelChange { .. } => "model_change",
            EntryData::ThinkingLevelChange { .. } => "thinking_level_change",
            EntryData::SessionInfo { .. } => "session_info",
            EntryData::Label { .. } => "label",
            EntryData::Compaction { .. } => "compaction",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn entries_serialize_flat_with_type_tags() {
        let entry = SessionEntry {
            id: "a1".into(),
            parent_id: None,
            timestamp: 5,
            data: EntryData::ModelChange {
                provider: "anthropic".into(),
                model_id: "claude".into(),
            },
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            value,
            json!({"id": "a1", "parentId": null, "timestamp": 5, "type": "model_change", "provider": "anthropic", "modelId": "claude"})
        );
        assert_eq!(
            serde_json::from_value::<SessionEntry>(value).unwrap(),
            entry
        );

        let message = SessionEntry {
            id: "b2".into(),
            parent_id: Some("a1".into()),
            timestamp: 6,
            data: EntryData::Message {
                message: AgentMessage::user("hi"),
                elapsed_seconds: None,
            },
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(
            serde_json::from_value::<SessionEntry>(value).unwrap(),
            message
        );

        let level = serde_json::json!({"id": "c", "parentId": "b2", "timestamp": 7, "type": "thinking_level_change", "thinkingLevel": "high"});
        let parsed: SessionEntry = serde_json::from_value(level).unwrap();
        assert_eq!(
            parsed.data,
            EntryData::ThinkingLevelChange {
                thinking_level: ThinkingLevel::High
            }
        );
    }
}
