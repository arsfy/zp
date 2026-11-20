//! Extension points of the agent loop.

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zp_ai::{AssistantMessage, Message, Tool, ToolCall, UserContent};

use crate::message::{AgentMessage, default_convert_to_llm};
use crate::tool::AgentToolResult;

/// What happens to a tool call after [`AgentHooks::before_tool_call`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolCallDecision {
    /// Execute the call.
    #[default]
    Allow,
    /// Do not execute it; the model receives an error result instead.
    Block {
        /// Shown to the model; a generic message when `None`.
        reason: Option<String>,
        /// Stop after this batch (only if every result of the batch asks for it).
        terminate: bool,
    },
}

impl ToolCallDecision {
    /// Blocks with `reason`.
    pub fn block(reason: impl Into<String>) -> Self {
        Self::Block {
            reason: Some(reason.into()),
            terminate: false,
        }
    }
}

/// The call being decided by [`AgentHooks::before_tool_call`].
#[derive(Clone, Copy, Debug)]
pub struct BeforeToolCall<'a> {
    /// The model and identity captured when the run started.
    pub run: &'a crate::ToolRun,
    /// The assistant message that requested the call.
    pub assistant_message: &'a AssistantMessage,
    /// The raw call.
    pub tool_call: &'a ToolCall,
    /// Validated arguments.
    pub args: &'a Value,
    /// The transcript at the time of the call.
    pub messages: &'a [AgentMessage],
}

/// The finished call passed to [`AgentHooks::after_tool_call`].
#[derive(Clone, Copy, Debug)]
pub struct AfterToolCall<'a> {
    /// The assistant message that requested the call.
    pub assistant_message: &'a AssistantMessage,
    /// The raw call.
    pub tool_call: &'a ToolCall,
    /// Validated arguments.
    pub args: &'a Value,
    /// The result as executed.
    pub result: &'a AgentToolResult,
    /// The transcript at the time of the call.
    pub messages: &'a [AgentMessage],
}

/// Field-by-field overrides of a tool result. `None` keeps the executed value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolResultOverride {
    /// Replaces the content.
    pub content: Option<Vec<UserContent>>,
    /// Replaces the details.
    pub details: Option<Value>,
    /// Replaces the error flag.
    pub is_error: Option<bool>,
    /// Replaces the termination hint.
    pub terminate: Option<bool>,
}

impl ToolResultOverride {
    pub(crate) fn apply(self, result: &mut AgentToolResult) {
        if let Some(content) = self.content {
            result.content = content;
        }
        if self.details.is_some() {
            result.details = self.details;
        }
        if let Some(is_error) = self.is_error {
            result.is_error = is_error;
        }
        if let Some(terminate) = self.terminate {
            result.terminate = terminate;
        }
    }
}

/// When the agent can compact its transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Before sending the next request.
    Threshold,
    /// After the current response attempt failed for lack of room: the provider rejected
    /// it, or stopped it at the length limit before it produced anything.
    ProviderError,
}

/// The assembled context just before a model request.
pub struct BeforeModelRequest<'a> {
    /// Whether the request is new or recovering from a provider error.
    pub trigger: CompactionTrigger,
    /// Model and request identity captured at the start of the run.
    pub run: &'a crate::ToolRun,
    /// Transcript including any new prompts and tool results.
    pub messages: &'a [AgentMessage],
    /// System prompt sent with this request.
    pub system_prompt: Option<&'a str>,
    /// Tool declarations sent with this request.
    pub tools: &'a [Tool],
    /// Call when summarizing actually starts (not for a check that finds nothing to
    /// do), so clients can show that the run is compacting.
    pub started: &'a (dyn Fn() + Send + Sync),
}

/// A committed compaction that replaces the agent's transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextCompaction {
    /// Identifies the compaction boundary for session-backed clients.
    pub entry_id: String,
    /// Summary and retained messages, in request order.
    pub messages: Vec<AgentMessage>,
    /// Estimated context size before compaction.
    pub tokens_before: u64,
    /// Estimated context size after compaction.
    pub tokens_after: u64,
}

/// Hooks into the agent loop. Every method has a no-op default.
///
/// Hooks receive the run's cancellation token and should honor it, e.g. when waiting
/// for the user to approve a tool call.
pub trait AgentHooks: Send + Sync {
    /// Optionally compacts and replaces the stored transcript before a model request.
    fn compact_context<'a>(
        &'a self,
        request: BeforeModelRequest<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<ContextCompaction>, String>> {
        let _ = (request, cancel);
        Box::pin(async { Ok(None) })
    }

    /// Rewrites the transcript before each model request (pruning, compaction, injected
    /// context). Return `None` to use it unchanged. The agent's stored transcript is not
    /// modified.
    fn transform_context<'a>(
        &'a self,
        messages: &'a [AgentMessage],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<Vec<AgentMessage>>> {
        let _ = (messages, cancel);
        Box::pin(async { None })
    }

    /// Converts the (transformed) transcript into model messages. The default keeps LLM
    /// messages and drops custom ones.
    fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        default_convert_to_llm(messages)
    }

    /// Decides whether a validated tool call may run (permission checks hook in here).
    fn before_tool_call<'a>(
        &'a self,
        call: BeforeToolCall<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolCallDecision> {
        let _ = (call, cancel);
        Box::pin(async { ToolCallDecision::Allow })
    }

    /// Adjusts an executed tool result before it is reported.
    fn after_tool_call<'a>(
        &'a self,
        call: AfterToolCall<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<ToolResultOverride>> {
        let _ = (call, cancel);
        Box::pin(async { None })
    }
}

/// Hooks that change nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl AgentHooks for NoHooks {}
