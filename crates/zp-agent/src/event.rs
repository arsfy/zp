//! Lifecycle events emitted by the agent.

use std::sync::Arc;

use serde_json::Value;
use zp_ai::{AssistantMessage, AssistantMessageEvent, ToolResultMessage};

use crate::hooks::{CompactionTrigger, ContextCompaction};
use crate::message::AgentMessage;
use crate::tool::AgentToolResult;

/// One step of an agent run. Every run emits:
///
/// ```text
/// AgentStart
///   TurnStart
///     MessageStart/End            prompt and queued messages
///     MessageStart (Update)* End  the assistant response
///     ToolExecutionStart (Update)* End, …
///     MessageStart/End            tool results, in call order
///   TurnEnd
///   … more turns while there are tool calls, steering or follow-up messages
/// AgentEnd
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    /// A run started.
    AgentStart,
    /// The run finished; no more events follow.
    AgentEnd {
        /// Every message the run added to the transcript.
        messages: Vec<AgentMessage>,
    },
    /// A turn (one model response plus its tool calls) started.
    TurnStart,
    /// The session started summarizing older messages before the next request.
    ContextCompacting {
        /// What triggered compaction.
        trigger: CompactionTrigger,
    },
    /// A committed compaction replaced the model's context before the request.
    ContextCompacted {
        /// The replacement context and its boundary.
        compaction: Arc<ContextCompaction>,
        /// What triggered compaction.
        trigger: CompactionTrigger,
    },
    /// Compaction failed and the existing context was preserved.
    ContextCompactionFailed {
        /// Failure reported by the summarizer or session store.
        error: String,
    },
    /// A turn finished.
    TurnEnd {
        /// The turn's assistant message.
        message: AssistantMessage,
        /// Results of its tool calls, in call order.
        tool_results: Vec<ToolResultMessage>,
    },
    /// A message started. For assistant messages this is the empty skeleton; content
    /// arrives through `MessageUpdate`s.
    MessageStart {
        /// The message (or its skeleton).
        message: AgentMessage,
    },
    /// The streaming assistant message changed. Fold the events with
    /// [`zp_ai::StreamAccumulator`], or read [`crate::Agent::streaming_message`].
    MessageUpdate {
        /// The provider event.
        event: AssistantMessageEvent,
    },
    /// A message is complete and appended to the transcript.
    MessageEnd {
        /// The final message.
        message: AgentMessage,
    },
    /// A tool call started (before permission checks).
    ToolExecutionStart {
        /// Call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Raw arguments from the model.
        args: Value,
    },
    /// A running tool reported progress.
    ToolExecutionUpdate {
        /// Call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// The partial result.
        partial_result: AgentToolResult,
    },
    /// A tool call finished (or was rejected without running).
    ToolExecutionEnd {
        /// Call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Final result after hooks.
        result: AgentToolResult,
        /// Whether the model sees it as an error.
        is_error: bool,
        /// How long `execute` ran; `None` if the tool did not run.
        duration_ms: Option<u64>,
    },
}

impl AgentEvent {
    /// The snake-case event name, matching pi's (`agent_start`, `tool_execution_end`, …).
    pub fn name(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::ContextCompacting { .. } => "context_compacting",
            Self::ContextCompacted { .. } => "context_compacted",
            Self::ContextCompactionFailed { .. } => "context_compaction_failed",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }
}
