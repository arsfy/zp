//! Unified LLM layer for ZP.
//!
//! - [`Models`] holds [`Provider`]s, resolves API keys and routes requests.
//! - [`Model`] describes a model: protocol, limits, pricing and reasoning support.
//! - [`Context`] / [`Message`] describe the conversation sent to a model.
//! - Responses stream as [`AssistantMessageEvent`]s ending in `Done` or `Error`;
//!   [`StreamAccumulator`] folds them into the partial message.
//!
//! Supported protocols: Anthropic Messages and OpenAI Chat Completions (and compatible
//! APIs). [`FauxProvider`] replays scripted responses for tests.
//!
//! Model catalogs are compiled in and can be refreshed at runtime from models.dev and
//! from each configured provider's model list; see [`sync`].

mod api;
mod auth;
mod catalog;
mod event;
mod faux;
mod http;
mod model;
mod models;
mod options;
pub mod partial_json;
mod provider;
mod sse;
pub mod sync;
mod transform;
mod types;

pub use auth::{
    AuthSource, CredentialStore, EnvLookup, MemoryCredentialStore, ResolvedAuth, process_env,
};
pub use catalog::builtin_providers;
pub use event::{
    AssistantMessageEvent, AssistantMessageStream, EventSink, StreamAccumulator, collect_message,
    error_stream, event_stream,
};
pub use faux::{FauxBlock, FauxProvider, FauxResponse};
pub use model::{
    Api, CostTier, InputModality, MaxTokensField, Model, ModelCompat, ModelCost, ThinkingFormat,
    ThinkingLevel,
};
pub use models::Models;
pub use options::{DEFAULT_MAX_RETRIES, StreamOptions, ThinkingBudgets};
pub use provider::{HttpProvider, Provider};
pub use sync::{
    MODELS_DEV_URL, ModelsDevCatalog, ModelsDevFetch, SyncError, UpstreamModel, UpstreamModels,
};
pub use types::{
    AssistantContent, AssistantMessage, Context, Cost, ImageContent, Message, StopReason,
    TextContent, ThinkingContent, Tool, ToolCall, ToolResultMessage, Usage, UserContent,
    UserMessage, now_ms,
};

/// Re-exported so callers can create cancellation tokens without another dependency.
pub use tokio_util::sync::CancellationToken;
