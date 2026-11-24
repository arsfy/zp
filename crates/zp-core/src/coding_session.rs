//! A coding session: an [`Agent`] with the built-in tools and project system prompt,
//! persisted to a [`Session`] file as it runs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context as _, bail};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zp_agent::{
    AfterToolCall, Agent, AgentError, AgentEvent, AgentHooks, AgentMessage, AgentOptions,
    AgentTool, BeforeModelRequest, BeforeToolCall, CompactionTrigger, ContextCompaction,
    PromptInput, Subscription, ToolCallDecision, ToolResultOverride,
};
use zp_ai::{Message, Model, Models, StopReason, StreamOptions, ThinkingLevel, Usage, UserContent};
use zp_session::{COMPACTION_SUMMARY_TYPE, EntryData, Session, SessionEntry};

use crate::changes::{ChangeLog, RestoreSource, restore_message_with_sources};
use crate::checkpoint::{BlobStore, RecordedChange, store_tool_change};
use crate::compaction::{self, CompactionSettings};
use crate::permissions::{PermissionGate, PermissionPolicy};
use crate::resources::Resources;
use crate::subagents::{self, SubagentSettings, SubagentTool};
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
    pub(crate) subagent: Option<Arc<SubagentTool>>,
    compaction: CompactionState,
}

impl AgentHooks for CodingHooks {
    fn compact_context<'a>(
        &'a self,
        request: BeforeModelRequest<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<ContextCompaction>, String>> {
        Box::pin(async move {
            let model = &request.run.model;
            let settings = &self.compaction.settings;
            if !settings.enabled {
                return Ok(None);
            }
            if request.trigger == CompactionTrigger::ProviderError
                && !request
                    .messages
                    .last()
                    .and_then(AgentMessage::as_assistant)
                    .is_some_and(|message| compaction::is_overflow(message, model))
            {
                return Ok(None);
            }
            let overhead = compaction::request_overhead(request.system_prompt, request.tools);
            let before = compaction::estimate_context(request.messages, Some(model), overhead);
            if request.trigger == CompactionTrigger::Threshold
                && !compaction::should_compact(before, model.context_window, settings)
            {
                return Ok(None);
            }
            self.compaction
                .perform(model, before, overhead, None, request.started, cancel)
                .await
                .map_err(|error| format!("Compaction failed: {error:#}"))
        })
    }

    fn before_tool_call<'a>(
        &'a self,
        call: BeforeToolCall<'a>,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolCallDecision> {
        Box::pin(async move {
            if call.tool_call.name == subagents::TOOL_NAME
                && let Some(subagent) = &self.subagent
            {
                return subagent.prepare(call, cancel).await;
            }
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
    /// Model to use; `None` uses the session's last model, if it has one (else the
    /// session starts without a model and runs fail until one is set).
    pub model: Option<Model>,
    /// Reasoning level; `None` uses the session's last level (or off).
    pub thinking_level: Option<ThinkingLevel>,
    /// Tools; `None` uses all built-in tools.
    pub tools: Option<Vec<Arc<dyn AgentTool>>>,
    /// Puts the agent's questions to the user; adds the `ask` tool.
    pub asker: Option<Arc<dyn zp_tools::Asker>>,
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
    /// The effective delegation policy.
    pub subagents: SubagentSettings,
    /// Samples the active run clock when an assistant response finishes.
    pub response_elapsed_seconds: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
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
            asker: None,
            global_context_dir: None,
            custom_system_prompt: None,
            append_system_prompt: None,
            request_options: StreamOptions::default(),
            blobs: None,
            permissions: None,
            trusted: true,
            compaction: CompactionSettings::default(),
            resources: Arc::default(),
            subagents: SubagentSettings::default(),
            response_elapsed_seconds: None,
        }
    }
}

/// The model a session last used, if it is still known.
pub fn session_model(models: &Models, session: &Session) -> Option<Model> {
    let reference = session.context().model?;
    models.model(&reference.provider, &reference.model_id)
}

/// Token and cost totals of a transcript.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
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
    /// Current context size when available.
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

    /// Adds one main response or recorded child task.
    pub fn add_message(&mut self, message: &AgentMessage) {
        if let Some(assistant) = message.as_assistant() {
            self.add(&assistant.usage);
        } else if let Some(result) = message.as_tool_result()
            && result.tool_name == subagents::TOOL_NAME
            && let Some(report) = result
                .details
                .as_ref()
                .and_then(subagents::Report::from_details)
        {
            self.add_subagent(&report.usage);
        }
    }

    /// Adds persisted usage, including requests no longer in the active model context.
    pub fn from_entries(entries: &[SessionEntry]) -> Self {
        let mut totals = Self::default();
        for entry in entries {
            if let Some(message) = entry.message() {
                totals.add_message(message);
            } else if let EntryData::Compaction {
                details: Some(details),
                ..
            } = &entry.data
                && let Some(usage) = details
                    .get("usage")
                    .and_then(|value| serde_json::from_value::<Self>(value.clone()).ok())
            {
                totals.add_subagent(&usage);
            }
        }
        totals
    }

    /// Adds a child task without changing the main model's context size.
    pub fn add_subagent(&mut self, child: &Self) {
        self.input += child.input;
        self.output += child.output;
        self.cache_read += child.cache_read;
        self.cache_write += child.cache_write;
        self.cost += child.cost;
    }
}

/// An agent bound to a project directory and a persisted session.
///
/// Every finished message is appended to the session as it happens; model and reasoning
/// level changes are recorded too, so reopening the session restores the conversation.
pub struct CodingSession {
    agent: Agent,
    models: Models,
    cwd: PathBuf,
    system_prompt: Mutex<String>,
    prompt_inputs: Mutex<PromptInputs>,
    blobs: BlobStore,
    permissions: Option<Arc<PermissionGate>>,
    asker: Option<Arc<dyn zp_tools::Asker>>,
    subagent: Mutex<Option<Arc<SubagentTool>>>,
    compaction: CompactionState,
    _persistence: Subscription,
}

#[derive(Clone)]
struct CompactionState {
    session: Arc<Mutex<Session>>,
    settings: CompactionSettings,
    /// Runs the summary requests.
    models: Models,
}

impl CompactionState {
    fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn perform(
        &self,
        model: &Model,
        before: u64,
        overhead: u64,
        instructions: Option<&str>,
        started: &(dyn Fn() + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<Option<ContextCompaction>> {
        let (head, rewind, preparation) = {
            let session = self.session();
            let mut path = session.branch_path(None);
            let failed = path
                .iter()
                .rposition(|entry| entry.message().is_some())
                .filter(|&index| {
                    path[index]
                        .message()
                        .and_then(AgentMessage::as_assistant)
                        .is_some_and(|message| {
                            matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
                                || compaction::is_overflow(message, model)
                        })
                });
            let rewind = failed.map(|index| {
                let parent = path[index].parent_id.clone();
                path.truncate(index);
                parent
            });
            (
                session.leaf_id().map(str::to_owned),
                rewind,
                compaction::prepare(&path, &self.settings),
            )
        };
        let Some(preparation) = preparation else {
            return Ok(None);
        };
        started();
        let result =
            compaction::summarize(&self.models, model, preparation, instructions, cancel).await?;
        let (entry_id, messages) = {
            let mut session = self.session();
            if cancel.is_cancelled() {
                bail!("compaction cancelled");
            }
            if session.leaf_id() != head.as_deref() {
                bail!("session changed while compacting");
            }
            if let Some(parent) = rewind {
                match parent {
                    Some(parent) => session.branch(&parent)?,
                    None => session.reset_leaf(),
                }
            }
            let entry_id = match session.append_compaction(
                result.summary,
                &result.first_kept_entry_id,
                before,
                Some(result.details),
            ) {
                Ok(id) => id,
                Err(error) => {
                    if let Some(head) = head {
                        session.branch(&head)?;
                    }
                    return Err(error);
                }
            };
            (entry_id, session.context().messages)
        };
        let tokens_after = compaction::estimate_context(&messages, Some(model), overhead);
        Ok(Some(ContextCompaction {
            entry_id,
            messages,
            tokens_before: before,
            tokens_after,
        }))
    }
}

/// `tools` plus `ask` when someone can answer.
fn with_session_tools(
    mut tools: Vec<Arc<dyn AgentTool>>,
    asker: Option<&Arc<dyn zp_tools::Asker>>,
    subagent: Option<&Arc<SubagentTool>>,
) -> Vec<Arc<dyn AgentTool>> {
    if let Some(asker) = asker
        && !tools.iter().any(|tool| tool.name() == "ask")
    {
        tools.push(Arc::new(zp_tools::AskTool::new(asker.clone())));
    }
    tools.retain(|tool| tool.name() != subagents::TOOL_NAME);
    if let Some(subagent) = subagent {
        tools.push(subagent.clone());
    }
    tools
}

impl CodingHooks {
    fn new(
        blobs: &BlobStore,
        permissions: Option<&Arc<PermissionGate>>,
        models: &Models,
        subagent: Option<&Arc<SubagentTool>>,
        compaction: &CompactionState,
    ) -> Arc<Self> {
        Arc::new(Self {
            blobs: blobs.clone(),
            permissions: permissions.cloned(),
            models: models.clone(),
            subagent: subagent.cloned(),
            compaction: compaction.clone(),
        })
    }
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
            asker,
            global_context_dir,
            custom_system_prompt,
            append_system_prompt,
            request_options,
            blobs,
            permissions,
            trusted,
            compaction,
            resources,
            subagents,
            response_elapsed_seconds,
        } = options;
        let blobs = blobs.unwrap_or_else(BlobStore::in_memory);
        let context = session.context();
        let model = model.or_else(|| session_model(&models, &session));
        if let Some(model) = &model {
            let recorded = context.model.as_ref();
            if recorded.is_none_or(|r| r.provider != model.provider || r.model_id != model.id) {
                session.append_model_change(&model.provider, &model.id)?;
            }
        }
        let thinking = thinking_level
            .or(context.thinking_level)
            .unwrap_or(ThinkingLevel::High);
        if context.thinking_level.unwrap_or(ThinkingLevel::Off) != thinking {
            session.append_thinking_level_change(thinking)?;
        }

        let subagent = subagents.is_usable().then(|| {
            Arc::new(SubagentTool::new(
                cwd.clone(),
                models.clone(),
                subagents,
                permissions.clone(),
            ))
        });
        let tools = with_session_tools(
            tools.unwrap_or_else(|| zp_tools::coding_tools(&cwd)),
            asker.as_ref(),
            subagent.as_ref(),
        );
        let context_files = context_files(&cwd, global_context_dir.as_deref(), trusted);
        let skills = resources.skills_prompt();
        let system_prompt = build_system_prompt(&SystemPromptOptions {
            cwd: &cwd,
            tools: &tools,
            context_files: &context_files,
            custom_prompt: custom_system_prompt.as_deref(),
            append: append_system_prompt.as_deref(),
            skills: Some(&skills),
            scratch_directory: permissions
                .as_deref()
                .and_then(PermissionGate::scratch_directory)
                .as_deref(),
        });

        let session_id = session.id().to_owned();
        let compaction = CompactionState {
            session: Arc::new(Mutex::new(session)),
            settings: compaction,
            models: models.clone(),
        };
        let mut agent_options = AgentOptions::with_models(models.clone(), model);
        agent_options.thinking_level = thinking;
        agent_options.system_prompt = Some(system_prompt.clone());
        agent_options.tools = tools;
        agent_options.messages = context.messages;
        agent_options.hooks = CodingHooks::new(
            &blobs,
            permissions.as_ref(),
            &models,
            subagent.as_ref(),
            &compaction,
        );
        agent_options.request_options = StreamOptions {
            session_id: Some(session_id),
            ..request_options
        };
        let agent = Agent::new(agent_options);

        let persistence = {
            let session = compaction.session.clone();
            agent.subscribe(move |event| {
                if let AgentEvent::MessageEnd { message } = event {
                    let elapsed_seconds = message
                        .as_assistant()
                        .and_then(|_| response_elapsed_seconds.as_ref().map(|elapsed| elapsed()));
                    let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Err(error) = session.append(EntryData::Message {
                        message: message.clone(),
                        elapsed_seconds,
                    }) {
                        tracing::error!(%error, "could not save message to the session");
                    }
                }
            })
        };
        Ok(Self {
            agent,
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
            asker,
            subagent: Mutex::new(subagent),
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
        self.compaction.session()
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
        let mut log = ChangeLog::from_path(&self.session().branch_path(None));
        log.resolve_reverts(Some(&self.blobs));
        log
    }

    /// The file changes along the branch ending at `leaf` (`None`: before any entry).
    pub fn change_log_at(&self, leaf: Option<&str>) -> ChangeLog {
        match leaf {
            Some(leaf) => {
                let mut log = ChangeLog::from_path(&self.session().branch_path(Some(leaf)));
                log.resolve_reverts(Some(&self.blobs));
                log
            }
            None => ChangeLog::default(),
        }
    }

    /// Records files the user restored (so the model knows), on the current branch.
    pub fn record_restore(
        &self,
        description: &str,
        changes: &[RecordedChange],
    ) -> anyhow::Result<()> {
        self.record_restore_with_sources(description, changes, &[])
    }

    pub fn record_restore_with_sources(
        &self,
        description: &str,
        changes: &[RecordedChange],
        sources: &[RestoreSource],
    ) -> anyhow::Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        if self.agent.is_streaming() {
            bail!("cannot record a restore while the agent is running");
        }
        let message = AgentMessage::Custom(restore_message_with_sources(
            description,
            changes,
            sources,
            &self.cwd,
        ));
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

    /// Updates permissions and the scratch directory advertised to the model.
    pub fn set_permission_policy(&self, policy: PermissionPolicy) {
        if let Some(permissions) = &self.permissions {
            permissions.set_policy(policy);
            self.rebuild_system_prompt();
        }
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

    /// Replaces the tools (e.g. MCP servers connected) and updates the system prompt;
    /// `ask` stays when there is an asker.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        let subagent = self.subagent.lock().unwrap_or_else(PoisonError::into_inner);
        self.agent.set_tools(with_session_tools(
            tools,
            self.asker.as_ref(),
            subagent.as_ref(),
        ));
        drop(subagent);
        self.rebuild_system_prompt();
    }

    /// Applies a new policy to future runs.
    pub fn set_subagents(&self, settings: SubagentSettings) {
        let enabled = settings.is_usable();
        let mut current = self.subagent.lock().unwrap_or_else(PoisonError::into_inner);
        if (enabled
            && current
                .as_ref()
                .is_some_and(|tool| tool.settings() == &settings))
            || (!enabled && current.is_none())
        {
            return;
        }
        *current = enabled.then(|| {
            Arc::new(SubagentTool::new(
                self.cwd.clone(),
                self.models.clone(),
                settings,
                self.permissions.clone(),
            ))
        });
        let tools = with_session_tools(self.agent.tools(), self.asker.as_ref(), current.as_ref());
        self.agent.set_tools_and_hooks(
            tools,
            CodingHooks::new(
                &self.blobs,
                self.permissions.as_ref(),
                &self.models,
                current.as_ref(),
                &self.compaction,
            ),
        );
        drop(current);
        self.rebuild_system_prompt();
    }

    /// Cancels a queued or running subagent.
    pub fn cancel_subagent(&self, call_id: &str) {
        if let Some(tool) = self
            .subagent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            tool.cancel(call_id);
        }
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
            scratch_directory: self
                .permissions
                .as_deref()
                .and_then(PermissionGate::scratch_directory)
                .as_deref(),
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
        if current
            .is_none_or(|current| current.provider != model.provider || current.id != model.id)
        {
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
        self.session().context_entry_ids_at(None)
    }

    /// Rewinds the failed or aborted response so [`Agent::continue_run`] can resume
    /// from the same context, then starts a new file change group for the retry.
    pub fn prepare_retry(&self) -> anyhow::Result<()> {
        if self.agent.is_streaming() {
            bail!("cannot retry while the agent is running");
        }
        let messages = {
            let mut session = self.session();
            let path = session.branch_path(None);
            let parent = path
                .iter()
                .rev()
                .find_map(|entry| Some((*entry, entry.message()?)))
                .filter(|(_, message)| {
                    message.as_assistant().is_some_and(|message| {
                        matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
                    })
                })
                .context("the last response did not fail")?
                .0
                .parent_id
                .clone();
            let previous_leaf = session.leaf_id().map(str::to_owned);
            match parent {
                Some(parent) => session.branch(&parent)?,
                None => session.reset_leaf(),
            }
            if let Err(error) = session.append(EntryData::Retry) {
                if let Some(previous_leaf) = previous_leaf {
                    session.branch(&previous_leaf)?;
                }
                return Err(error);
            }
            session.context().messages
        };
        self.agent.set_messages(messages);
        Ok(())
    }

    fn request_overhead(&self) -> u64 {
        let tools = self
            .agent
            .tools()
            .iter()
            .map(|tool| tool.declaration())
            .collect::<Vec<_>>();
        compaction::request_overhead(self.agent.system_prompt().as_deref(), &tools)
    }

    /// Estimated size of the current context in tokens.
    pub fn context_tokens(&self) -> u64 {
        compaction::estimate_context(
            &self.agent.messages(),
            self.agent.model().as_ref(),
            self.request_overhead(),
        )
    }

    /// Whether the current transcript is near the model's limit. Runs compact by
    /// themselves before a request; this is for showing it (and for tests).
    pub fn needs_compaction(&self) -> bool {
        self.agent.model().is_some_and(|model| {
            compaction::should_compact(
                self.context_tokens(),
                model.context_window,
                &self.compaction.settings,
            )
        })
    }

    /// Whether the last response ran out of context room. Runs compact and retry once by
    /// themselves; this tells a client that the retry did not help either.
    pub fn overflowed(&self) -> bool {
        let Some(model) = self.agent.model() else {
            return false;
        };
        self.agent
            .messages()
            .iter()
            .rev()
            .find_map(AgentMessage::as_assistant)
            .is_some_and(|message| compaction::is_overflow(message, &model))
    }

    /// Summarizes older messages into a compaction entry. Returns `(tokens before,
    /// tokens after)`, or `None` when there was nothing to compact.
    pub async fn compact(&self, instructions: Option<&str>) -> anyhow::Result<Option<(u64, u64)>> {
        if self.agent.is_streaming() {
            bail!("cannot compact while the agent is running");
        }
        let model = self.agent.model().context("no model is selected")?;
        let overhead = self.request_overhead();
        let before = compaction::estimate_context(&self.agent.messages(), Some(&model), overhead);
        let Some(result) = self
            .compaction
            .perform(
                &model,
                before,
                overhead,
                instructions,
                &|| {},
                &CancellationToken::new(),
            )
            .await?
        else {
            return Ok(None);
        };
        self.agent.set_messages(result.messages);
        Ok(Some((result.tokens_before, result.tokens_after)))
    }

    /// Total recorded usage and current context size.
    pub fn usage(&self) -> UsageTotals {
        let mut totals = UsageTotals::from_entries(self.session().entries());
        totals.last_context_tokens = self.context_tokens();
        totals
    }

    /// Cache-read ratio reported by the latest main-model request on this branch.
    pub fn last_cache_read_rate(&self) -> Option<f64> {
        self.session()
            .branch_path(None)
            .iter()
            .rev()
            .filter_map(|entry| entry.message().and_then(AgentMessage::as_assistant))
            // Failed requests report no input; look past them.
            .find_map(|assistant| assistant.usage.cache_read_rate())
    }
}
