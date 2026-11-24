//! Stateful agent loop for ZP, modeled on pi-agent-core.
//!
//! - [`Agent`] owns the transcript, model, thinking level and tools, runs the loop on
//!   [`Agent::prompt`] / [`Agent::continue_run`], and reports progress as [`AgentEvent`]s
//!   (`agent_start/end`, `turn_start/end`, `message_start/update/end`,
//!   `tool_execution_start/update/end`).
//! - [`AgentTool`] is a tool the model can call; arguments are validated against its JSON
//!   Schema (with lenient coercion) before it runs.
//! - Tool calls of one response run in parallel or sequentially ([`ToolExecutionMode`]).
//! - [`AgentHooks`] covers `transform_context` / `convert_to_llm` (compaction, custom
//!   messages) and `before_tool_call` / `after_tool_call` (permissions, result rewriting).
//! - While a run is active, [`Agent::steer`] queues messages for after the current turn,
//!   [`Agent::follow_up`] for after the agent would stop, and [`Agent::abort`] cancels it.

mod agent;
mod agent_loop;
mod event;
mod hooks;
mod message;
mod schema;
mod tool;

pub use agent::{
    Agent, AgentError, AgentEvents, AgentOptions, AgentState, PromptInput, QueueMode, SteeringId,
    Subscription, stream_fn,
};
pub use agent_loop::StreamFn;
pub use event::AgentEvent;
pub use hooks::{
    AfterToolCall, AgentHooks, BeforeModelRequest, BeforeToolCall, CompactionTrigger,
    ContextCompaction, NoHooks, ToolCallDecision, ToolResultOverride,
};
pub use message::{AgentMessage, CustomMessage, CustomRole, default_convert_to_llm};
pub use schema::validate_arguments;
pub use tool::{AgentTool, AgentToolResult, ToolContext, ToolExecutionMode, ToolRun};
