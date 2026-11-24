//! The stateful agent: transcript, queues, abort, and event subscription.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context as TaskContext, Poll};

use futures::channel::mpsc;
use futures::{FutureExt as _, Stream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zp_ai::{
    AssistantMessage, Message, Model, Models, StopReason, StreamAccumulator, StreamOptions,
    ThinkingLevel, UserMessage,
};

use crate::agent_loop::{
    Emit, LoopConfig, LoopQueues, StreamFn, failed_message, panic_message, run_loop,
};
use crate::event::AgentEvent;
use crate::hooks::{AgentHooks, NoHooks};
use crate::message::{AgentMessage, CustomMessage};
use crate::tool::{AgentTool, ToolExecutionMode, ToolRun};

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

/// Adapts [`Models`] (credential resolution and provider routing) into a [`StreamFn`].
pub fn stream_fn(models: Models) -> StreamFn {
    Arc::new(move |model, context, options| models.stream(model, context, options))
}

/// How many queued messages are taken at each drain point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    /// Every queued message.
    All,
    /// Only the oldest; the rest wait for later drain points.
    #[default]
    OneAtATime,
}

/// Identifies one steering message until the agent takes it from the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteeringId(u64);

/// Why a call was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentError {
    /// A run is in progress; use [`Agent::steer`] or [`Agent::follow_up`], or wait.
    Busy,
    /// `prompt` was called with no messages.
    EmptyPrompt,
    /// `continue_run` was called on an empty transcript.
    NothingToContinue,
    /// `continue_run` was called after an assistant message with nothing queued.
    CannotContinueFromAssistant,
    /// The requested steering message was already taken or removed.
    SteeringMessageUnavailable,
    /// No model is selected.
    NoModel,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "the agent is already running",
            Self::EmptyPrompt => "the prompt has no messages",
            Self::NothingToContinue => "there are no messages to continue from",
            Self::CannotContinueFromAssistant => {
                "cannot continue after an assistant message without queued messages"
            }
            Self::NoModel => "no model is selected",
            Self::SteeringMessageUnavailable => "the queued message is no longer available",
        })
    }
}

impl std::error::Error for AgentError {}

/// Messages to start a run with.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptInput(pub Vec<AgentMessage>);

impl From<&str> for PromptInput {
    fn from(text: &str) -> Self {
        Self(vec![AgentMessage::user(text)])
    }
}

impl From<String> for PromptInput {
    fn from(text: String) -> Self {
        Self(vec![AgentMessage::user(text)])
    }
}

impl From<AgentMessage> for PromptInput {
    fn from(message: AgentMessage) -> Self {
        Self(vec![message])
    }
}

impl From<Message> for PromptInput {
    fn from(message: Message) -> Self {
        Self(vec![message.into()])
    }
}

impl From<UserMessage> for PromptInput {
    fn from(message: UserMessage) -> Self {
        Self(vec![message.into()])
    }
}

impl From<CustomMessage> for PromptInput {
    fn from(message: CustomMessage) -> Self {
        Self(vec![message.into()])
    }
}

impl From<Vec<AgentMessage>> for PromptInput {
    fn from(messages: Vec<AgentMessage>) -> Self {
        Self(messages)
    }
}

/// Construction options for [`Agent`].
pub struct AgentOptions {
    /// Produces model responses.
    pub stream: StreamFn,
    /// Model for future runs; runs fail with [`AgentError::NoModel`] until one is set.
    pub model: Option<Model>,
    /// Requested reasoning level (clamped per model by the provider layer).
    pub thinking_level: ThinkingLevel,
    /// System prompt.
    pub system_prompt: Option<String>,
    /// Executable tools; their declarations are sent with every request.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Initial transcript (e.g. a restored session).
    pub messages: Vec<AgentMessage>,
    /// Loop hooks.
    pub hooks: Arc<dyn AgentHooks>,
    /// How tool calls of one message run.
    pub tool_execution: ToolExecutionMode,
    /// Drain policy of the steering queue.
    pub steering_mode: QueueMode,
    /// Drain policy of the follow-up queue.
    pub follow_up_mode: QueueMode,
    /// Template for every request (session id, max tokens, headers…). `thinking` and
    /// `cancel` are set by the agent.
    pub request_options: StreamOptions,
}

impl AgentOptions {
    /// Options with defaults: no tools, no hooks, parallel tools, one-at-a-time queues.
    pub fn new(stream: StreamFn, model: impl Into<Option<Model>>) -> Self {
        Self {
            stream,
            model: model.into(),
            thinking_level: ThinkingLevel::Off,
            system_prompt: None,
            tools: Vec::new(),
            messages: Vec::new(),
            hooks: Arc::new(NoHooks),
            tool_execution: ToolExecutionMode::default(),
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
            request_options: StreamOptions::default(),
        }
    }

    /// Options streaming through `models`.
    pub fn with_models(models: Models, model: impl Into<Option<Model>>) -> Self {
        Self::new(stream_fn(models), model)
    }
}

/// A snapshot of the agent's state.
#[derive(Clone)]
pub struct AgentState {
    /// System prompt.
    pub system_prompt: Option<String>,
    /// Model for future runs, if one is selected.
    pub model: Option<Model>,
    /// Requested reasoning level.
    pub thinking_level: ThinkingLevel,
    /// Executable tools.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Completed transcript messages.
    pub messages: Vec<AgentMessage>,
    /// Whether a run is in progress.
    pub is_streaming: bool,
    /// The assistant message being streamed, if any.
    pub streaming_message: Option<AssistantMessage>,
    /// Ids of tool calls currently running.
    pub pending_tool_calls: BTreeSet<String>,
    /// Error of the most recent failed or aborted response.
    pub error_message: Option<String>,
}

impl fmt::Debug for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools: Vec<&str> = self.tools.iter().map(|tool| tool.name()).collect();
        f.debug_struct("AgentState")
            .field("model", &self.model.as_ref().map(Model::qualified_id))
            .field("thinking_level", &self.thinking_level)
            .field("tools", &tools)
            .field("messages", &self.messages.len())
            .field("is_streaming", &self.is_streaming)
            .field("pending_tool_calls", &self.pending_tool_calls)
            .field("error_message", &self.error_message)
            .finish_non_exhaustive()
    }
}

type Listener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

struct State {
    system_prompt: Option<String>,
    model: Option<Model>,
    thinking_level: ThinkingLevel,
    tools: Vec<Arc<dyn AgentTool>>,
    messages: Vec<AgentMessage>,
    streaming: Option<StreamAccumulator>,
    pending_tool_calls: BTreeSet<String>,
    error_message: Option<String>,
    run: Option<CancellationToken>,
}

struct Settings {
    stream: StreamFn,
    hooks: Arc<dyn AgentHooks>,
    tool_execution: ToolExecutionMode,
    request_options: StreamOptions,
}

struct Queue {
    messages: VecDeque<QueuedMessage>,
    mode: QueueMode,
    next_id: u64,
}

struct QueuedMessage {
    id: SteeringId,
    message: AgentMessage,
    held: bool,
}

impl Queue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: VecDeque::new(),
            mode,
            next_id: 0,
        }
    }

    fn push(&mut self, message: AgentMessage) -> SteeringId {
        let id = SteeringId(self.next_id);
        self.next_id += 1;
        self.messages.push_back(QueuedMessage {
            id,
            message,
            held: false,
        });
        id
    }

    fn take(&mut self, id: SteeringId) -> Option<AgentMessage> {
        let index = self.messages.iter().position(|entry| entry.id == id)?;
        self.messages.remove(index).map(|entry| entry.message)
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        let limit = match self.mode {
            QueueMode::All => usize::MAX,
            QueueMode::OneAtATime => 1,
        };
        let count = self
            .messages
            .iter()
            .take(limit)
            .take_while(|entry| !entry.held)
            .count();
        self.messages
            .drain(..count)
            .map(|entry| entry.message)
            .collect()
    }
}

struct Queues {
    steering: Queue,
    follow_up: Queue,
}

struct Shared {
    state: Mutex<State>,
    settings: Mutex<Settings>,
    queues: Mutex<Queues>,
    listeners: Mutex<Vec<(u64, Listener)>>,
    next_listener: AtomicU64,
    running: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Folds `event` into the state, then notifies listeners outside the lock (so they
    /// may call back into the agent).
    fn emit(&self, event: AgentEvent) {
        {
            let mut state = lock(&self.state);
            match &event {
                AgentEvent::MessageStart {
                    message: AgentMessage::Llm(Message::Assistant(message)),
                } => state.streaming = Some(StreamAccumulator::new(message.clone())),
                AgentEvent::MessageUpdate { event } => {
                    if let Some(streaming) = state.streaming.as_mut() {
                        streaming.apply(event);
                    }
                }
                AgentEvent::MessageEnd { message } => {
                    state.streaming = None;
                    state.messages.push(message.clone());
                }
                AgentEvent::ContextCompacted { compaction, .. } => {
                    state.messages.clone_from(&compaction.messages);
                    state.error_message = None;
                }
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                    state.pending_tool_calls.insert(tool_call_id.clone());
                }
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                    state.pending_tool_calls.remove(tool_call_id);
                }
                AgentEvent::TurnEnd { message, .. } => {
                    if let Some(error) = &message.error_message {
                        state.error_message = Some(error.clone());
                    }
                }
                AgentEvent::AgentEnd { .. } => state.streaming = None,
                _ => {}
            }
        }
        let listeners: Vec<Listener> = lock(&self.listeners)
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| listener(&event))) {
                tracing::error!(
                    event = event.name(),
                    panic = panic_message(panic.as_ref()),
                    "agent event listener panicked"
                );
            }
        }
    }
}

/// Marks the agent busy for the lifetime of a run; dropping it (even when the run's
/// future is dropped) makes the agent idle again.
struct RunGuard {
    shared: Arc<Shared>,
    cancel: CancellationToken,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared.state);
            state.run = None;
            state.streaming = None;
            state.pending_tool_calls.clear();
        }
        self.shared.running.send_replace(false);
    }
}

/// A stateful agent: owns the transcript, runs the loop, and reports progress as
/// [`AgentEvent`]s.
///
/// `Agent` is a cheap handle (`Clone` shares the same agent), so one task can run
/// [`Agent::prompt`] while others steer, queue follow-ups or abort:
///
/// ```no_run
/// # async fn demo(agent: zp_agent::Agent) {
/// let runner = agent.clone();
/// let run = tokio::spawn(async move { runner.prompt("Fix the failing test").await });
/// agent.steer("Only touch src/");
/// agent.abort();
/// let _ = run.await;
/// # }
/// ```
#[derive(Clone)]
pub struct Agent {
    shared: Arc<Shared>,
}

impl fmt::Debug for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.state().fmt(f)
    }
}

impl Agent {
    /// Creates an idle agent.
    pub fn new(options: AgentOptions) -> Self {
        let (running, _) = watch::channel(false);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    system_prompt: options.system_prompt,
                    model: options.model,
                    thinking_level: options.thinking_level,
                    tools: options.tools,
                    messages: options.messages,
                    streaming: None,
                    pending_tool_calls: BTreeSet::new(),
                    error_message: None,
                    run: None,
                }),
                settings: Mutex::new(Settings {
                    stream: options.stream,
                    hooks: options.hooks,
                    tool_execution: options.tool_execution,
                    request_options: options.request_options,
                }),
                queues: Mutex::new(Queues {
                    steering: Queue::new(options.steering_mode),
                    follow_up: Queue::new(options.follow_up_mode),
                }),
                listeners: Mutex::new(Vec::new()),
                next_listener: AtomicU64::new(0),
                running,
            }),
        }
    }

    // ----- state -------------------------------------------------------------------

    /// A snapshot of the whole state.
    pub fn state(&self) -> AgentState {
        let state = lock(&self.shared.state);
        AgentState {
            system_prompt: state.system_prompt.clone(),
            model: state.model.clone(),
            thinking_level: state.thinking_level,
            tools: state.tools.clone(),
            messages: state.messages.clone(),
            is_streaming: state.run.is_some(),
            streaming_message: state.streaming.as_ref().map(|s| s.message().clone()),
            pending_tool_calls: state.pending_tool_calls.clone(),
            error_message: state.error_message.clone(),
        }
    }

    /// The completed transcript.
    pub fn messages(&self) -> Vec<AgentMessage> {
        lock(&self.shared.state).messages.clone()
    }

    /// Replaces the transcript (takes effect for the next run).
    pub fn set_messages(&self, messages: Vec<AgentMessage>) {
        lock(&self.shared.state).messages = messages;
    }

    /// The model for future runs, if one is selected.
    pub fn model(&self) -> Option<Model> {
        lock(&self.shared.state).model.clone()
    }

    /// Sets the model for future runs.
    pub fn set_model(&self, model: Model) {
        lock(&self.shared.state).model = Some(model);
    }

    /// The requested reasoning level.
    pub fn thinking_level(&self) -> ThinkingLevel {
        lock(&self.shared.state).thinking_level
    }

    /// Sets the reasoning level for future runs.
    pub fn set_thinking_level(&self, level: ThinkingLevel) {
        lock(&self.shared.state).thinking_level = level;
    }

    /// The system prompt.
    pub fn system_prompt(&self) -> Option<String> {
        lock(&self.shared.state).system_prompt.clone()
    }

    /// Sets the system prompt for future runs.
    pub fn set_system_prompt(&self, prompt: Option<String>) {
        lock(&self.shared.state).system_prompt = prompt;
    }

    /// The executable tools.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        lock(&self.shared.state).tools.clone()
    }

    /// Sets the tools for future runs.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        lock(&self.shared.state).tools = tools;
    }

    /// Replaces the tools and their hooks together for future runs.
    pub fn set_tools_and_hooks(&self, tools: Vec<Arc<dyn AgentTool>>, hooks: Arc<dyn AgentHooks>) {
        let mut state = lock(&self.shared.state);
        let mut settings = lock(&self.shared.settings);
        state.tools = tools;
        settings.hooks = hooks;
    }

    /// Sets the hooks for future runs.
    pub fn set_hooks(&self, hooks: Arc<dyn AgentHooks>) {
        lock(&self.shared.settings).hooks = hooks;
    }

    /// Sets the response source for future runs.
    pub fn set_stream_fn(&self, stream: StreamFn) {
        lock(&self.shared.settings).stream = stream;
    }

    /// Sets the tool execution mode for future runs.
    pub fn set_tool_execution(&self, mode: ToolExecutionMode) {
        lock(&self.shared.settings).tool_execution = mode;
    }

    /// Sets the request template for future runs.
    pub fn set_request_options(&self, options: StreamOptions) {
        lock(&self.shared.settings).request_options = options;
    }

    /// Sets the cache policy for subsequent runs, keeping other request settings.
    pub fn set_cache_retention(&self, retention: zp_ai::CacheRetention) {
        lock(&self.shared.settings).request_options.cache_retention = retention;
    }

    /// Whether a run is in progress.
    pub fn is_streaming(&self) -> bool {
        lock(&self.shared.state).run.is_some()
    }

    /// The assistant message being streamed, if any.
    pub fn streaming_message(&self) -> Option<AssistantMessage> {
        lock(&self.shared.state)
            .streaming
            .as_ref()
            .map(|s| s.message().clone())
    }

    /// Ids of tool calls currently running.
    pub fn pending_tool_calls(&self) -> BTreeSet<String> {
        lock(&self.shared.state).pending_tool_calls.clone()
    }

    /// Error of the most recent failed or aborted response.
    pub fn error_message(&self) -> Option<String> {
        lock(&self.shared.state).error_message.clone()
    }

    /// Clears the transcript, queues and error. Fails while running.
    pub fn reset(&self) -> Result<(), AgentError> {
        {
            let mut state = lock(&self.shared.state);
            if state.run.is_some() {
                return Err(AgentError::Busy);
            }
            state.messages.clear();
            state.streaming = None;
            state.pending_tool_calls.clear();
            state.error_message = None;
        }
        self.clear_queues();
        Ok(())
    }

    // ----- events ------------------------------------------------------------------

    /// Calls `listener` synchronously for every event, in order, until the returned
    /// [`Subscription`] is dropped. The agent's state already reflects the event when the
    /// listener runs. Keep listeners fast; forward to a channel for slow work.
    pub fn subscribe(
        &self,
        listener: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> Subscription {
        let id = self.shared.next_listener.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.listeners).push((id, Arc::new(listener)));
        Subscription {
            shared: Arc::downgrade(&self.shared),
            id,
        }
    }

    /// Events as an async stream; unsubscribes when dropped.
    pub fn events(&self) -> AgentEvents {
        let (sender, receiver) = mpsc::unbounded();
        let subscription = self.subscribe(move |event| {
            let _ = sender.unbounded_send(event.clone());
        });
        AgentEvents {
            receiver,
            _subscription: subscription,
        }
    }

    // ----- queues ------------------------------------------------------------------

    /// Queues a message to insert after the current turn's tool calls finish.
    pub fn steer(&self, message: impl Into<AgentMessage>) {
        self.enqueue_steering(message);
    }

    /// Queues a steering message and returns its identifier for subsequent edits.
    pub fn enqueue_steering(&self, message: impl Into<AgentMessage>) -> SteeringId {
        lock(&self.shared.queues).steering.push(message.into())
    }

    /// Queues a message to process once the agent would otherwise stop.
    pub fn follow_up(&self, message: impl Into<AgentMessage>) {
        lock(&self.shared.queues).follow_up.push(message.into());
    }

    /// Queued steering messages, oldest first.
    pub fn steering_queue(&self) -> Vec<AgentMessage> {
        lock(&self.shared.queues)
            .steering
            .messages
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    /// Identifiers of the steering messages still waiting, in queue order.
    pub fn steering_ids(&self) -> Vec<SteeringId> {
        lock(&self.shared.queues)
            .steering
            .messages
            .iter()
            .map(|entry| entry.id)
            .collect()
    }

    /// Holds a message in place until it can be edited or sent. A held message also
    /// keeps later messages in order. Returns false once the agent has taken it.
    pub fn hold_steering(&self, id: SteeringId, held: bool) -> bool {
        let mut queues = lock(&self.shared.queues);
        let Some(entry) = queues
            .steering
            .messages
            .iter_mut()
            .find(|entry| entry.id == id)
        else {
            return false;
        };
        entry.held = held;
        true
    }

    /// Holds the selected message at the front so the current run cannot take it or
    /// later messages while winding down for an interruption.
    pub fn prioritize_steering(&self, id: SteeringId) -> bool {
        let mut queues = lock(&self.shared.queues);
        let messages = &mut queues.steering.messages;
        let Some(index) = messages.iter().position(|entry| entry.id == id) else {
            return false;
        };
        let mut entry = messages.remove(index).expect("the entry is still queued");
        entry.held = true;
        messages.push_front(entry);
        true
    }

    /// Replaces a waiting message in place and releases its hold.
    pub fn replace_steering(&self, id: SteeringId, message: AgentMessage) -> bool {
        let mut queues = lock(&self.shared.queues);
        let Some(entry) = queues
            .steering
            .messages
            .iter_mut()
            .find(|entry| entry.id == id)
        else {
            return false;
        };
        entry.message = message;
        entry.held = false;
        true
    }

    /// Withdraws a message if it is still queued.
    pub fn remove_steering(&self, id: SteeringId) -> bool {
        lock(&self.shared.queues).steering.take(id).is_some()
    }

    /// Starts a run with the selected steering message. Other queued messages wait
    /// for subsequent turns even if this message was held or behind them.
    pub async fn prompt_steering(&self, id: SteeringId) -> Result<Vec<AgentMessage>, AgentError> {
        let guard = self.begin_run(CancellationToken::new())?;
        let message = lock(&self.shared.queues)
            .steering
            .take(id)
            .ok_or(AgentError::SteeringMessageUnavailable)?;
        Ok(self.execute(guard, vec![message], true).await)
    }

    /// Queued follow-up messages, oldest first.
    pub fn follow_up_queue(&self) -> Vec<AgentMessage> {
        lock(&self.shared.queues)
            .follow_up
            .messages
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    /// Whether either queue holds messages.
    pub fn has_queued_messages(&self) -> bool {
        let queues = lock(&self.shared.queues);
        !queues.steering.messages.is_empty() || !queues.follow_up.messages.is_empty()
    }

    /// Drops all steering messages.
    pub fn clear_steering_queue(&self) {
        lock(&self.shared.queues).steering.messages.clear();
    }

    /// Drops all follow-up messages.
    pub fn clear_follow_up_queue(&self) {
        lock(&self.shared.queues).follow_up.messages.clear();
    }

    /// Drops all queued messages.
    pub fn clear_queues(&self) {
        let mut queues = lock(&self.shared.queues);
        queues.steering.messages.clear();
        queues.follow_up.messages.clear();
    }

    /// Sets how steering messages are drained.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.shared.queues).steering.mode = mode;
    }

    /// Sets how follow-up messages are drained.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.shared.queues).follow_up.mode = mode;
    }

    // ----- runs --------------------------------------------------------------------

    /// Aborts the current run, if any. The in-flight request and tools are cancelled;
    /// the run ends with an aborted assistant message. Queued messages are kept.
    pub fn abort(&self) {
        if let Some(run) = &lock(&self.shared.state).run {
            run.cancel();
        }
    }

    /// Waits until no run is in progress.
    pub async fn wait_for_idle(&self) {
        let mut running = self.shared.running.subscribe();
        let _ = running.wait_for(|running| !running).await;
    }

    /// Appends `input` and runs until the agent stops. Returns the messages the run
    /// added (including `input`).
    ///
    /// Dropping the returned future stops the run without an `AgentEnd` event; prefer
    /// [`Agent::abort`].
    pub async fn prompt(
        &self,
        input: impl Into<PromptInput>,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        self.prompt_with_cancel(input, CancellationToken::new())
            .await
    }

    /// Runs a prompt with a supplied cancellation token, for a scoped child agent.
    pub async fn prompt_with_cancel(
        &self,
        input: impl Into<PromptInput>,
        cancel: CancellationToken,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        let PromptInput(messages) = input.into();
        if messages.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        let guard = self.begin_run(cancel)?;
        Ok(self.execute(guard, messages, false).await)
    }

    /// Continues from the current transcript, e.g. after an error or to retry. If the
    /// last message is from the assistant, queued steering (or else follow-up) messages
    /// are used as the prompt.
    pub async fn continue_run(&self) -> Result<Vec<AgentMessage>, AgentError> {
        let guard = self.begin_run(CancellationToken::new())?;
        let last_is_assistant = {
            let state = lock(&self.shared.state);
            match state.messages.last() {
                None => return Err(AgentError::NothingToContinue),
                Some(message) => message.is_assistant(),
            }
        };
        if !last_is_assistant {
            return Ok(self.execute(guard, Vec::new(), false).await);
        }
        let steering = lock(&self.shared.queues).steering.drain();
        if !steering.is_empty() {
            return Ok(self.execute(guard, steering, true).await);
        }
        let follow_up = lock(&self.shared.queues).follow_up.drain();
        if !follow_up.is_empty() {
            return Ok(self.execute(guard, follow_up, false).await);
        }
        Err(AgentError::CannotContinueFromAssistant)
    }

    fn begin_run(&self, cancel: CancellationToken) -> Result<RunGuard, AgentError> {
        {
            let mut state = lock(&self.shared.state);
            if state.run.is_some() {
                return Err(AgentError::Busy);
            }
            if state.model.is_none() {
                return Err(AgentError::NoModel);
            }
            state.run = Some(cancel.clone());
            state.streaming = None;
            state.error_message = None;
        }
        self.shared.running.send_replace(true);
        Ok(RunGuard {
            shared: self.shared.clone(),
            cancel,
        })
    }

    async fn execute(
        &self,
        guard: RunGuard,
        prompts: Vec<AgentMessage>,
        skip_initial_steering: bool,
    ) -> Vec<AgentMessage> {
        let (config, context) = {
            let state = lock(&self.shared.state);
            let settings = lock(&self.shared.settings);
            let config = LoopConfig {
                run: Arc::new(ToolRun {
                    id: NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed),
                    // Checked by `begin_run`; a model cannot be unset.
                    model: state.model.clone().expect("a run has a model"),
                    thinking_level: state.thinking_level,
                }),
                system_prompt: state.system_prompt.clone(),
                tools: state.tools.clone(),
                tool_execution: settings.tool_execution,
                stream: settings.stream.clone(),
                request_options: settings.request_options.clone(),
                hooks: settings.hooks.clone(),
            };
            (config, state.messages.clone())
        };
        let emit: Emit = {
            let shared = self.shared.clone();
            Arc::new(move |event| shared.emit(event))
        };
        let mut queues = {
            let steering_shared = self.shared.clone();
            let follow_up_shared = self.shared.clone();
            let mut skip = skip_initial_steering;
            LoopQueues {
                steering: Box::new(move || {
                    if std::mem::take(&mut skip) {
                        return Vec::new();
                    }
                    lock(&steering_shared.queues).steering.drain()
                }),
                follow_up: Box::new(move || lock(&follow_up_shared.queues).follow_up.drain()),
            }
        };

        let outcome = AssertUnwindSafe(run_loop(
            &config,
            context,
            prompts,
            &mut queues,
            &emit,
            &guard.cancel,
        ))
        .catch_unwind()
        .await;
        match outcome {
            Ok(messages) => messages,
            Err(panic) => {
                // Close the run with a failed turn so listeners see a complete sequence.
                let reason = if guard.cancel.is_cancelled() {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                let error = format!("agent loop panicked: {}", panic_message(panic.as_ref()));
                tracing::error!(%error);
                let message = failed_message(&config.run.model, reason, &error);
                emit(AgentEvent::MessageStart {
                    message: message.clone().into(),
                });
                emit(AgentEvent::MessageEnd {
                    message: message.clone().into(),
                });
                emit(AgentEvent::TurnEnd {
                    message: message.clone(),
                    tool_results: Vec::new(),
                });
                let messages = vec![message.into()];
                emit(AgentEvent::AgentEnd {
                    messages: messages.clone(),
                });
                messages
            }
        }
    }
}

/// Keeps a listener registered; dropping it unsubscribes.
#[must_use = "dropping the subscription unsubscribes the listener"]
pub struct Subscription {
    shared: Weak<Shared>,
    id: u64,
}

impl Subscription {
    /// Keeps the listener for the agent's lifetime.
    pub fn detach(self) {
        std::mem::forget(self);
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            lock(&shared.listeners).retain(|(id, _)| *id != self.id);
        }
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .finish()
    }
}

/// A stream of [`AgentEvent`]s from [`Agent::events`].
pub struct AgentEvents {
    receiver: mpsc::UnboundedReceiver<AgentEvent>,
    _subscription: Subscription,
}

impl Stream for AgentEvents {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}

impl From<&str> for AgentMessage {
    fn from(text: &str) -> Self {
        Self::user(text)
    }
}

impl From<String> for AgentMessage {
    fn from(text: String) -> Self {
        Self::user(text)
    }
}
