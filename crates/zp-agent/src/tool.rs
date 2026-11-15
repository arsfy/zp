//! Tools the agent can execute.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zp_ai::{Tool, UserContent};

/// How the tool calls of one assistant message run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// One at a time: each call is checked, executed and finalized before the next.
    Sequential,
    /// Calls are checked one by one, then the allowed ones execute concurrently.
    /// `ToolExecutionEnd` events arrive in completion order; tool result messages are
    /// still emitted in the order the model requested them.
    #[default]
    Parallel,
}

/// A tool the agent can run.
///
/// Implementations return a boxed future so the trait stays object-safe:
///
/// ```
/// use futures::future::BoxFuture;
/// use serde_json::{Value, json};
/// use zp_agent::{AgentTool, AgentToolResult, ToolContext};
///
/// struct Echo;
///
/// impl AgentTool for Echo {
///     fn name(&self) -> &str { "echo" }
///     fn description(&self) -> &str { "Echo the input." }
///     fn parameters(&self) -> Value {
///         json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
///     }
///     fn execute<'a>(&'a self, args: Value, _ctx: ToolContext)
///         -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
///         Box::pin(async move { Ok(AgentToolResult::text(args["text"].as_str().unwrap_or_default())) })
///     }
/// }
/// ```
pub trait AgentTool: Send + Sync {
    /// Unique name the model calls the tool by.
    fn name(&self) -> &str;

    /// Human-readable label for the UI.
    fn label(&self) -> &str {
        self.name()
    }

    /// Description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema of the arguments (`{"type": "object", ...}`).
    fn parameters(&self) -> Value;

    /// One-line summary for the system prompt's tool list; tools without one are not
    /// listed there (the model still sees their declaration).
    fn prompt_snippet(&self) -> Option<&str> {
        None
    }

    /// Usage rules added to the system prompt when the tool is enabled.
    fn prompt_guidelines(&self) -> &[&str] {
        &[]
    }

    /// Forces an execution mode for batches containing this tool. `Sequential` makes the
    /// whole batch sequential.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Adjusts raw arguments before validation (e.g. for renamed parameters).
    fn prepare_arguments(&self, arguments: Value) -> Value {
        arguments
    }

    /// Runs the tool with validated arguments. Return `Err` (or a result with `is_error`)
    /// on failure; the model then sees the message as an error result. Honor
    /// `ctx.cancel` for long-running work.
    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>>;

    /// The declaration sent to the model.
    fn declaration(&self) -> Tool {
        Tool {
            name: self.name().to_owned(),
            description: self.description().to_owned(),
            parameters: self.parameters(),
        }
    }
}

/// The (partial or final) result of a tool call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentToolResult {
    /// Content returned to the model.
    pub content: Vec<UserContent>,
    /// Structured data for the UI and logs; stored on the tool result message, never sent
    /// to the model.
    pub details: Option<Value>,
    /// Report a failure without returning `Err`, keeping `details`.
    pub is_error: bool,
    /// Asks the agent to stop after this batch. Only honored when every result of the
    /// batch asks for it.
    pub terminate: bool,
}

impl AgentToolResult {
    /// A text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![UserContent::text(text)],
            ..Self::default()
        }
    }

    /// A text error result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }

    /// Sets `details`.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Per-call context handed to [`AgentTool::execute`].
#[derive(Clone)]
pub struct ToolContext {
    /// Id of the tool call being executed.
    pub tool_call_id: String,
    /// Cancelled when the run is aborted.
    pub cancel: CancellationToken,
    updates: Arc<UpdateSink>,
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("tool_call_id", &self.tool_call_id)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

struct UpdateSink {
    open: AtomicBool,
    emit: Box<dyn Fn(AgentToolResult) + Send + Sync>,
}

impl ToolContext {
    pub(crate) fn new(
        tool_call_id: String,
        cancel: CancellationToken,
        emit: impl Fn(AgentToolResult) + Send + Sync + 'static,
    ) -> Self {
        Self {
            tool_call_id,
            cancel,
            updates: Arc::new(UpdateSink {
                open: AtomicBool::new(true),
                emit: Box::new(emit),
            }),
        }
    }

    /// A context for calling a tool outside the agent (e.g. in tests); updates are
    /// dropped.
    pub fn detached(tool_call_id: impl Into<String>, cancel: CancellationToken) -> Self {
        Self::new(tool_call_id.into(), cancel, |_| {})
    }

    /// Streams a partial result (e.g. command output so far). Ignored once the call has
    /// finished.
    pub fn update(&self, partial: AgentToolResult) {
        if self.updates.open.load(Ordering::Acquire) {
            (self.updates.emit)(partial);
        }
    }

    /// Stops forwarding updates.
    pub(crate) fn close(&self) {
        self.updates.open.store(false, Ordering::Release);
    }
}
