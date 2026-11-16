//! A coding session: an [`Agent`] with the built-in tools and project system prompt,
//! persisted to a [`Session`] file as it runs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context as _, bail};
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use zp_agent::{
    AfterToolCall, Agent, AgentError, AgentEvent, AgentHooks, AgentMessage, AgentOptions,
    AgentTool, BeforeToolCall, PromptInput, Subscription, ToolCallDecision, ToolResultOverride,
};
use zp_ai::{Message, Model, Models, StopReason, StreamOptions, ThinkingLevel, Usage, UserContent};
use zp_session::{COMPACTION_SUMMARY_TYPE, Session};

use crate::changes::{ChangeLog, restore_message};
use crate::checkpoint::{BlobStore, RecordedChange, store_tool_change};
use crate::compaction::CompactionSettings;
use crate::permissions::PermissionGate;
use crate::resources::Resources;
use crate::system_prompt::{SystemPromptOptions, build_system_prompt, load_context_files};

const COMPACTION_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUFFIX: &str = "\n</summary>";

/// Shows custom transcript messages to the model as user messages (compaction summaries
/// are wrapped so the model knows what they are), checks tool calls against the
/// permissions, and moves file contents of `write` / `edit` results into the snapshot
/// store.
#[derive(Clone)]
pub struct CodingHooks {
    /// Where file versions are kept.
    pub blobs: BlobStore,
    /// Permission checks; `None` allows every call.
    pub permissions: Option<Arc<PermissionGate>>,
    /// Resolves the model of a call (for auto-review).
    pub models: Models,
}

impl AgentHooks for CodingHooks {
    fn before_tool_call<'a>(
        &'a self,
        call: BeforeToolCall<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolCallDecision> {
        Box::pin(async move {
            let Some(gate) = &self.permissions else {
                return ToolCallDecision::Allow;
            };
            let message = call.assistant_message;
            let model = self.models.model(&message.provider, &message.model);
            gate.check(
                call.tool_call,
                call.args,
                model.as_ref(),
                call.messages,
                cancel,
            )
            .await
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        call: AfterToolCall<'a>,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<ToolResultOverride>> {
        let details = call
            .result
            .details
            .as_ref()
            .and_then(|details| store_tool_change(&self.blobs, details));
        Box::pin(async move {
            match details? {
                Ok(details) => Some(ToolResultOverride {
                    details: Some(details),
                    ..ToolResultOverride::default()
                }),
                Err(error) => {
                    // Keep the inline contents rather than losing the change.
                    tracing::warn!(%error, "could not store file snapshots");
                    None
                }
            }
        })
    }

    fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Llm(message) => Some(message.clone()),
                AgentMessage::Custom(custom) if custom.content.is_empty() => None,
                AgentMessage::Custom(custom) => {
                    let mut user = zp_ai::UserMessage {
                        content: custom.content.clone(),
                        timestamp: custom.timestamp,
                    };
                    if custom.custom_type == COMPACTION_SUMMARY_TYPE {
                        let summary: String = custom
                            .content
                            .iter()
                            .filter_map(UserContent::as_text)
                            .collect();
                        user.content = vec![UserContent::text(format!(
                            "{COMPACTION_PREFIX}{summary}{COMPACTION_SUFFIX}"
                        ))];
                    }
                    Some(Message::User(user))
                }
            })
            .collect()
    }
}

/// Options for [`CodingSession::new`].
pub struct CodingSessionOptions {
    /// Providers and credentials.
    pub models: Models,
    /// Project directory; tools resolve paths against it.
    pub cwd: PathBuf,
    /// The session to continue (new or opened).
    pub session: Session,
    /// Model to use; `None` uses the session's last model.
    pub model: Option<Model>,
    /// Reasoning level; `None` uses the session's last level (or off).
    pub thinking_level: Option<ThinkingLevel>,
    /// Tools; `None` uses all built-in tools.
    pub tools: Option<Vec<Arc<dyn AgentTool>>>,
    /// Directory with a global `AGENTS.md` (usually the data directory).
    pub global_context_dir: Option<PathBuf>,
    /// Replaces the default system prompt body.
    pub custom_system_prompt: Option<String>,
    /// Appended to the system prompt.
    pub append_system_prompt: Option<String>,
    /// Request template (max tokens, headers…).
    pub request_options: StreamOptions,
    /// Where file versions are kept; `None` keeps them in memory.
    pub blobs: Option<BlobStore>,
    /// Permission checks; `None` allows every tool call.
    pub permissions: Option<Arc<PermissionGate>>,
    /// Whether the project is trusted: its instruction files are only loaded then.
    pub trusted: bool,
    /// When to compact the context.
    pub compaction: CompactionSettings,
    /// Prompt templates and skills.
    pub resources: Arc<Resources>,
}

impl CodingSessionOptions {
    /// Options with defaults for everything but the essentials.
    pub fn new(models: Models, cwd: impl Into<PathBuf>, session: Session) -> Self {
        Self {
            models,
            cwd: cwd.into(),
            session,
            model: None,
            thinking_level: None,
            tools: None,
            global_context_dir: None,
            custom_system_prompt: None,
            append_system_prompt: None,
            request_options: StreamOptions::default(),
            blobs: None,
            permissions: None,
            trusted: true,
            compaction: CompactionSettings::default(),
            resources: Arc::default(),
        }
    }
}

/// The model a session last used, if it is still known.
pub fn session_model(models: &Models, session: &Session) -> Option<Model> {
    let reference = session.context().model?;
    models.model(&reference.provider, &reference.model_id)
}

/// Token and cost totals of a transcript.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageTotals {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache reads.
    pub cache_read: u64,
    /// Cache writes.
    pub cache_write: u64,
    /// Cost in US dollars.
    pub cost: f64,
    /// Input of the last successful response (≈ current context size).
    pub last_context_tokens: u64,
}

impl UsageTotals {
    /// Adds one response's usage.
    pub fn add(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
    }
}

/// An agent bound to a project directory and a persisted session.
///
/// Every finished message is appended to the session as it happens; model and reasoning
/// level changes are recorded too, so reopening the session restores the conversation.
pub struct CodingSession {
    agent: Agent,
    session: Arc<Mutex<Session>>,
    models: Models,
    cwd: PathBuf,
    system_prompt: Mutex<String>,
    prompt_inputs: Mutex<PromptInputs>,
    blobs: BlobStore,
    permissions: Option<Arc<PermissionGate>>,
    compaction: CompactionSettings,
    _persistence: Subscription,
}

/// What the system prompt is built from (to rebuild it when trust, resources or tools
/// change).
struct PromptInputs {
    global_context_dir: Option<PathBuf>,
    custom_system_prompt: Option<String>,
    append_system_prompt: Option<String>,
    trusted: bool,
    resources: Arc<Resources>,
}

/// Context files of `cwd`; without trust only the global one.
fn context_files(
    cwd: &Path,
    global_dir: Option<&Path>,
    trusted: bool,
) -> Vec<crate::system_prompt::ContextFile> {
    let files = load_context_files(cwd, global_dir);
    if trusted {
        return files;
    }
    let boundary = crate::project::trust_boundary(cwd);
    files
        .into_iter()
        .filter(|file| {
            !file.path.starts_with(&boundary)
                || global_dir.is_some_and(|g| file.path.starts_with(g))
        })
        .collect()
}

impl CodingSession {
    /// Restores the session's transcript into a new agent.
    pub fn new(options: CodingSessionOptions) -> anyhow::Result<Self> {
        let CodingSessionOptions {
            models,
            cwd,
            mut session,
            model,
            thinking_level,
            tools,
            global_context_dir,
            custom_system_prompt,
            append_system_prompt,
            request_options,
            blobs,
            permissions,
            trusted,
            compaction,
            resources,
        } = options;
        let blobs = blobs.unwrap_or_else(BlobStore::in_memory);
        let context = session.context();
        let model = match model {
            Some(model) => model,
            None => session_model(&models, &session)
                .context("no model selected and the session has no usable model")?,
        };
        let recorded = context.model.as_ref();
        if recorded.is_none_or(|r| r.provider != model.provider || r.model_id != model.id) {
            session.append_model_change(&model.provider, &model.id)?;
        }
        let thinking = thinking_level
            .or(context.thinking_level)
            .unwrap_or(ThinkingLevel::Off);
        if context.thinking_level.unwrap_or(ThinkingLevel::Off) != thinking {
            session.append_thinking_level_change(thinking)?;
        }

        let tools = tools.unwrap_or_else(|| zp_tools::coding_tools(&cwd));
        let context_files = context_files(&cwd, global_context_dir.as_deref(), trusted);
        let skills = resources.skills_prompt();
        let system_prompt = build_system_prompt(&SystemPromptOptions {
            cwd: &cwd,
            tools: &tools,
            context_files: &context_files,
            custom_prompt: custom_system_prompt.as_deref(),
            append: append_system_prompt.as_deref(),
            skills: Some(&skills),
        });

        let mut agent_options = AgentOptions::with_models(models.clone(), model);
        agent_options.thinking_level = thinking;
        agent_options.system_prompt = Some(system_prompt.clone());
        agent_options.tools = tools;
        agent_options.messages = context.messages;
        agent_options.hooks = Arc::new(CodingHooks {
            blobs: blobs.clone(),
            permissions: permissions.clone(),
            models: models.clone(),
        });
        agent_options.request_options = StreamOptions {
            session_id: Some(session.id().to_owned()),
            ..request_options
        };
        let agent = Agent::new(agent_options);

        let session = Arc::new(Mutex::new(session));
        let persistence = {
            let session = session.clone();
            agent.subscribe(move |event| {
                if let AgentEvent::MessageEnd { message } = event {
                    let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Err(error) = session.append_message(message.clone()) {
                        tracing::error!(%error, "could not save message to the session");
                    }
                }
            })
        };
        Ok(Self {
            agent,
            session,
            models,
            cwd,
            system_prompt: Mutex::new(system_prompt),
            prompt_inputs: Mutex::new(PromptInputs {
                global_context_dir,
                custom_system_prompt,
                append_system_prompt,
                trusted,
                resources,
            }),
            blobs,
            permissions,
            compaction,
            _persistence: persistence,
        })
    }

    /// The agent (for events, steering, abort, state).
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// The session. Do not hold the guard across `.await`.
    pub fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Providers and credentials.
    pub fn models(&self) -> &Models {
        &self.models
    }

    /// Project directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Where file versions are kept.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// The file changes along the current branch.
    pub fn change_log(&self) -> ChangeLog {
        ChangeLog::from_path(&self.session().branch_path(None))
    }

    /// The file changes along the branch ending at `leaf` (`None`: before any entry).
    pub fn change_log_at(&self, leaf: Option<&str>) -> ChangeLog {
        match leaf {
            Some(leaf) => ChangeLog::from_path(&self.session().branch_path(Some(leaf))),
            None => ChangeLog::default(),
        }
    }

    /// Records files the user restored (so the model knows), on the current branch.
    pub fn record_restore(
        &self,
        description: &str,
        changes: &[RecordedChange],
    ) -> anyhow::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        if self.agent.is_streaming() {
            bail!("cannot record a restore while the agent is running");
        }
        let message = AgentMessage::Custom(restore_message(description, changes, &self.cwd));
        let messages = {
            let mut session = self.session();
            session.append_message(message)?;
            session.context().messages
        };
        self.agent.set_messages(messages);
        Ok(())
    }

    /// The system prompt in use.
    pub fn system_prompt(&self) -> String {
        self.system_prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Permission checks, if any.
    pub fn permissions(&self) -> Option<&Arc<PermissionGate>> {
        self.permissions.as_ref()
    }

    /// Rebuilds the system prompt with or without the project's instruction files (after
    /// the user decided whether to trust the project). Applies from the next request.
    pub fn set_trusted(&self, trusted: bool) {
        self.prompt_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .trusted = trusted;
        self.rebuild_system_prompt();
    }

    /// Replaces the prompt templates and skills (e.g. after files changed).
    pub fn set_resources(&self, resources: Arc<Resources>) {
        self.prompt_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resources = resources;
        self.rebuild_system_prompt();
    }

    /// The prompt templates and skills in use.
    pub fn resources(&self) -> Arc<Resources> {
        self.prompt_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resources
            .clone()
    }

    /// Replaces the tools (e.g. MCP servers connected) and updates the system prompt.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        self.agent.set_tools(tools);
        self.rebuild_system_prompt();
    }

    /// Expands a `/template` or `/skill:name` command; other input is returned as is.
    pub fn expand_input(&self, input: &str) -> String {
        self.resources()
            .expand(input.trim_start())
            .unwrap_or_else(|| input.to_owned())
    }

    fn rebuild_system_prompt(&self) {
        let inputs = self
            .prompt_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let tools = self.agent.tools();
        let files = context_files(
            &self.cwd,
            inputs.global_context_dir.as_deref(),
            inputs.trusted,
        );
        let skills = inputs.resources.skills_prompt();
        let prompt = build_system_prompt(&SystemPromptOptions {
            cwd: &self.cwd,
            tools: &tools,
            context_files: &files,
            custom_prompt: inputs.custom_system_prompt.as_deref(),
            append: inputs.append_system_prompt.as_deref(),
            skills: Some(&skills),
        });
        drop(inputs);
        self.agent.set_system_prompt(Some(prompt.clone()));
        *self
            .system_prompt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = prompt;
    }

    /// Runs a prompt to completion (see [`Agent::prompt`]).
    pub async fn prompt(
        &self,
        input: impl Into<PromptInput>,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        self.agent.prompt(input).await
    }

    /// Switches the model for future runs and records it.
    pub fn set_model(&self, model: Model) -> anyhow::Result<()> {
        let current = self.agent.model();
        if current.provider != model.provider || current.id != model.id {
            self.session()
                .append_model_change(&model.provider, &model.id)?;
        }
        self.agent.set_model(model);
        Ok(())
    }

    /// Changes the reasoning level for future runs and records it.
    pub fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()> {
        if self.agent.thinking_level() != level {
            self.session().append_thinking_level_change(level)?;
        }
        self.agent.set_thinking_level(level);
        Ok(())
    }

    /// Sets the session's display name.
    pub fn set_name(&self, name: Option<String>) -> anyhow::Result<()> {
        self.session().append_session_info(name)?;
        Ok(())
    }

    /// Continues from entry `id`: the next prompt starts a new branch there.
    pub fn branch(&self, id: &str) -> anyhow::Result<()> {
        if self.agent.is_streaming() {
            bail!("cannot branch while the agent is running");
        }
        let messages = {
            let mut session = self.session();
            session.branch(id)?;
            session.context().messages
        };
        self.agent.set_messages(messages);
        Ok(())
    }

    /// Moves to just before entry `id` (e.g. to edit and resend a message): the next prompt
    /// starts a sibling branch of `id`.
    pub fn branch_before(&self, id: &str) -> anyhow::Result<()> {
        if self.agent.is_streaming() {
            bail!("cannot branch while the agent is running");
        }
        let messages = {
            let mut session = self.session();
            let parent = session
                .entry(id)
                .with_context(|| format!("entry {id} not found"))?
                .parent_id
                .clone();
            match parent {
                Some(parent) => session.branch(&parent)?,
                None => session.reset_leaf(),
            }
            session.context().messages
        };
        self.agent.set_messages(messages);
        Ok(())
    }

    /// Switches to the most recent branch through entry `id`.
    pub fn switch_branch(&self, id: &str) -> anyhow::Result<()> {
        let leaf = self
            .session()
            .latest_leaf_under(id)
            .map(str::to_owned)
            .with_context(|| format!("entry {id} not found"))?;
        self.branch(&leaf)
    }

    /// The session entry behind each message of the agent transcript.
    pub fn context_entry_ids(&self) -> Vec<String> {
        self.session().context().entry_ids
    }

    /// Rewinds past a failed or aborted last response so that
    /// [`Agent::continue_run`] asks again. The failed response stays in the session file
    /// on an abandoned branch.
    pub fn prepare_retry(&self) -> anyhow::Result<()> {
        if self.agent.is_streaming() {
            bail!("cannot retry while the agent is running");
        }
        let messages = {
            let mut session = self.session();
            let path = session.branch_path(None);
            let failed = path
                .iter()
                .rev()
                .find_map(|entry| Some((entry, entry.message()?)))
                .filter(|(_, message)| {
                    message.as_assistant().is_some_and(|m| {
                        matches!(m.stop_reason, StopReason::Error | StopReason::Aborted)
                    })
                })
                .map(|(entry, _)| entry.parent_id.clone());
            match failed {
                Some(Some(parent)) => session.branch(&parent)?,
                Some(None) => session.reset_leaf(),
                None => bail!("the last response did not fail"),
            }
            session.context().messages
        };
        self.agent.set_messages(messages);
        Ok(())
    }

    /// Estimated size of the current context in tokens.
    pub fn context_tokens(&self) -> u64 {
        crate::compaction::context_tokens(&self.agent.messages())
    }

    /// Whether the context is close enough to the window to compact before the next
    /// request.
    pub fn needs_compaction(&self) -> bool {
        crate::compaction::should_compact(
            self.context_tokens(),
            self.agent.model().context_window,
            &self.compaction,
        )
    }

    /// Whether the last response failed because the context was too large.
    pub fn overflowed(&self) -> bool {
        self.agent
            .messages()
            .iter()
            .rev()
            .find_map(AgentMessage::as_assistant)
            .is_some_and(|message| {
                message.stop_reason == StopReason::Error
                    && message
                        .error_message
                        .as_deref()
                        .is_some_and(crate::compaction::is_context_overflow)
            })
    }

    /// Summarizes older messages into a compaction entry. Returns `(tokens before,
    /// tokens after)`, or `None` when there was nothing to compact.
    pub async fn compact(&self, instructions: Option<&str>) -> anyhow::Result<Option<(u64, u64)>> {
        if self.agent.is_streaming() {
            bail!("cannot compact while the agent is running");
        }
        // A failed attempt (e.g. a context overflow) is dropped from the context first.
        let failed = self
            .agent
            .messages()
            .last()
            .and_then(AgentMessage::as_assistant)
            .is_some_and(|m| matches!(m.stop_reason, StopReason::Error | StopReason::Aborted));
        if failed {
            let _ = self.prepare_retry();
        }
        let preparation = {
            let session = self.session();
            crate::compaction::prepare(&session.branch_path(None), &self.compaction)
        };
        let Some(preparation) = preparation else {
            return Ok(None);
        };
        let before = self.context_tokens();
        let model = self.agent.model();
        let result =
            crate::compaction::summarize(&self.models, &model, preparation, instructions).await?;
        let messages = {
            let mut session = self.session();
            session.append_compaction(
                result.summary,
                &result.first_kept_entry_id,
                before,
                Some(result.details),
            )?;
            session.context().messages
        };
        self.agent.set_messages(messages);
        // Reported usage predates the summary; estimate the new context instead.
        let after = self
            .agent
            .messages()
            .iter()
            .map(crate::compaction::estimate_tokens)
            .sum();
        Ok(Some((before, after)))
    }

    /// Usage of the assistant messages in the current transcript.
    pub fn usage(&self) -> UsageTotals {
        let mut totals = UsageTotals::default();
        for message in self.agent.messages() {
            if let Some(assistant) = message.as_assistant() {
                totals.add(&assistant.usage);
                if assistant.stop_reason != StopReason::Error
                    && assistant.stop_reason != StopReason::Aborted
                {
                    totals.last_context_tokens = assistant.usage.total_tokens;
                }
            }
        }
        totals
    }
}
