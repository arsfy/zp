//! A coding session: an [`Agent`] with the built-in tools and project system prompt,
//! persisted to a [`Session`] file as it runs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context as _, bail};
use zp_agent::{
    Agent, AgentError, AgentEvent, AgentHooks, AgentMessage, AgentOptions, AgentTool, PromptInput,
    Subscription,
};
use zp_ai::{Message, Model, Models, StopReason, StreamOptions, ThinkingLevel, Usage, UserContent};
use zp_session::{COMPACTION_SUMMARY_TYPE, Session};

use crate::system_prompt::{SystemPromptOptions, build_system_prompt, load_context_files};

const COMPACTION_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUFFIX: &str = "\n</summary>";

/// Shows custom transcript messages to the model as user messages; compaction summaries
/// are wrapped so the model knows what they are.
#[derive(Clone, Copy, Debug, Default)]
pub struct CodingHooks;

impl AgentHooks for CodingHooks {
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
    fn add(&mut self, usage: &Usage) {
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
    system_prompt: String,
    _persistence: Subscription,
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
        } = options;
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
        let context_files = load_context_files(&cwd, global_context_dir.as_deref());
        let system_prompt = build_system_prompt(&SystemPromptOptions {
            cwd: &cwd,
            tools: &tools,
            context_files: &context_files,
            custom_prompt: custom_system_prompt.as_deref(),
            append: append_system_prompt.as_deref(),
        });

        let mut agent_options = AgentOptions::with_models(models.clone(), model);
        agent_options.thinking_level = thinking;
        agent_options.system_prompt = Some(system_prompt.clone());
        agent_options.tools = tools;
        agent_options.messages = context.messages;
        agent_options.hooks = Arc::new(CodingHooks);
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
            system_prompt,
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

    /// The system prompt in use.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
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
