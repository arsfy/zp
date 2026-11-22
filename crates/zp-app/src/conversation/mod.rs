//! A conversation: one coding session shown as a transcript with a composer.
//!
//! The agent runs on Tokio; its events are forwarded through a channel and applied to the
//! [`Transcript`] in batches, at most ~30 times per second, so streaming does not redraw
//! for every token.

pub mod composer;
mod disclosure;
mod files;
mod permissions;
mod progress;
mod questions;
mod render;
mod temp_files;
pub mod transcript;
pub mod tree;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, ScrollHandle, Subscription, Task, Window, px,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, TextChanged};
use zp_agent::AgentEvent;
use zp_ai::{Model, ThinkingLevel};
use zp_core::{CodingSession, CodingSessionOptions, UsageTotals, session_model};
use zp_session::Session;

use self::transcript::{Changes, MessageTiming, Transcript};
use crate::actions::{
    ClosePicker, PickSelectedModel, SelectNextModel, SelectPreviousModel, SendMessage, StopRun,
};
use crate::diff_view::DiffItem;
use crate::markdown::{Block, parse};
use crate::review_model_picker::ReviewModelPicker;
use crate::selectable_text::TextSelection;
use crate::store::{AppModels, AppPaths};

/// Minimum time between applying event batches.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Status updates are less frequent than streamed text and stop when the task ends.
const STATUS_INTERVAL: Duration = Duration::from_millis(250);
/// Corner radius of the composer and the Tasks card above it.
const COMPOSER_CARD_RADIUS: f32 = 8.0;
/// Shortest time a tool card or thinking block stays open when it opened only because it
/// was in progress (the "collapse when done" display): quick commands and short thoughts
/// would otherwise flash open and shut.
pub(crate) const MIN_EXPANDED: Duration = Duration::from_millis(1500);

/// An agent event with its arrival time and any saved message timing.
struct ReceivedEvent {
    at: Instant,
    event: AgentEvent,
    timing: Option<MessageTiming>,
}

impl ReceivedEvent {
    fn new(at: Instant, event: AgentEvent) -> Self {
        Self {
            at,
            event,
            timing: None,
        }
    }
}

/// A transcript block that opens while it is in progress.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HeldBlock {
    /// A tool card, by call id.
    Tool(String),
    /// A thinking block of a response row.
    Thinking { row: usize, block: usize },
}

/// When a block was first shown open because it was in progress.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Held {
    pub(crate) since: Instant,
    /// Its collapse has been scheduled.
    pub(crate) collapsing: bool,
}

/// A session handed over to the window (e.g. a fork to open in a new tab).
pub type SessionHandoff = Rc<RefCell<Option<Session>>>;

/// Events for the window around the conversation.
#[derive(Clone)]
pub enum ConversationEvent {
    /// Model, reasoning level, usage or running state changed.
    StatusChanged,
    /// The session file was written for the first time.
    SessionCreated,
    /// A run finished (the session file changed).
    RunFinished,
    /// The session was named.
    TitleChanged,
    /// The user started working in this conversation (a preview tab becomes permanent).
    Activity,
    /// A fork of this conversation should open in a new tab.
    Forked(SessionHandoff),
    /// Files were changed (by a tool or a restore), or another branch is shown.
    FilesChanged,
    /// A user-confirmed restore finished; a diff can return to the conversation.
    FilesRestored,
    /// The diffs to show, and the initially selected file.
    OpenDiff { items: Vec<DiffItem>, index: usize },
    /// The user picked a permission mode (for the whole project).
    PermissionModeChanged(zp_core::permissions::PermissionMode),
    /// A `/` command the window handles.
    Command(composer::WindowCommand),
    /// A tool call waits for the user (its summary).
    ApprovalRequested(String),
    /// The agent asks the user something (the first question).
    QuestionAsked(String),
}

/// A sent user message being edited in place.
pub(crate) struct MessageEdit {
    pub(crate) row: usize,
    pub(crate) input: Entity<EditableTextState>,
    _changed: Subscription,
}

/// Which popover is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Picker {
    Model,
    Thinking,
    Permissions,
}

/// Status shown in the window's status bar.
#[derive(Clone, Debug)]
pub struct ConversationStatus {
    pub model: Option<Model>,
    /// Shown under the composer rather than in the status bar.
    #[cfg_attr(not(test), allow(dead_code))]
    pub thinking: ThinkingLevel,
    pub usage: UsageTotals,
    /// Cached input fraction of the most recent main-model request.
    pub cache_read_rate: Option<f64>,
    pub running: bool,
    /// Whether the in-flight output usage is estimated until provider usage arrives.
    pub output_estimated: bool,
    /// Output speed of the current or last streamed response.
    pub tokens_per_second: Option<f64>,
}

/// When the streaming response started and produced its first output.
#[derive(Clone, Copy, Debug, Default)]
struct ResponseTimer {
    started: Option<Instant>,
    first_output: Option<Instant>,
    output_bytes: u64,
    reported_output: u64,
    reported_bytes: u64,
}

impl ResponseTimer {
    fn estimated_output(&self) -> u64 {
        // Match the existing rough token estimate, counting only new bytes after a
        // provider update so that reported counts recalibrate the live estimate.
        self.reported_output + (self.output_bytes - self.reported_bytes).div_ceil(4)
    }

    fn speed(&self, at: Instant, output: u64) -> Option<f64> {
        let seconds = at
            .duration_since(self.first_output.or(self.started)?)
            .as_secs_f64();
        (output > 0 && seconds >= 0.05).then(|| output as f64 / seconds)
    }
}

/// A model offered by the model picker.
#[derive(Clone, Debug)]
pub struct ModelChoice {
    pub model: Model,
    pub provider_name: String,
}

pub struct ConversationView {
    text_selection: TextSelection,
    root: PathBuf,
    coding: Option<Arc<CodingSession>>,
    unavailable: Option<String>,
    transcript: Transcript,
    list_state: ListState,
    running: bool,
    /// Error from the last attempt to start a run.
    run_error: Option<String>,
    composer: Entity<EditableTextState>,
    /// Sent prompts, before template expansion, and the position being recalled.
    prompt_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    model_filter: Entity<EditableTextState>,
    /// The model picker's highlighted row (moved with the arrow keys, picked with enter).
    model_selected: usize,
    /// Scrolls the highlighted model into view.
    model_scroll: ScrollHandle,
    picker: Option<Picker>,
    pub(crate) review_model_picker: Option<Entity<ReviewModelPicker>>,
    review_model_subscription: Option<Subscription>,
    /// When a popover was last closed by clicking outside it; a click on its own toggle
    /// button right after must not reopen it.
    picker_closed_at: Option<std::time::Instant>,
    /// Thinking blocks the user toggled, by (row, block) (overrides the default).
    thinking_expanded: HashMap<(usize, usize), bool>,
    /// Tool cards the user toggled, by call id (overrides the default).
    tool_expanded: HashMap<String, bool>,
    /// Blocks shown open because they were in progress. Those that collapse when done stay
    /// open for [`MIN_EXPANDED`] at least.
    pub(crate) held_open: RefCell<HashMap<HeldBlock, Held>>,
    /// Open and close transitions of collapsible blocks.
    disclosures: disclosure::Disclosures,
    /// Held blocks that were redrawn to collapse (tests).
    #[cfg(test)]
    block_collapses: usize,
    /// The transcript display settings rows were last measured with.
    display: zp_core::settings::TranscriptSettings,
    markdown_cache: RefCell<HashMap<u64, Rc<Vec<Block>>>>,
    /// Diffs of tool calls, by call id.
    tool_diffs: RefCell<HashMap<String, Rc<Result<files::ToolDiff, String>>>>,
    /// Net file changes for completed answers, by transcript row.
    turn_changes: RefCell<HashMap<usize, Option<Rc<files::TurnChanges>>>>,
    /// Rows whose full list of changed files is open.
    turn_files_expanded: HashSet<usize>,
    /// Subagent calls whose whole conclusion is shown.
    full_subagent_results: HashSet<String>,
    change_log_cache: RefCell<Option<Rc<zp_core::changes::ChangeLog>>>,
    /// Tool calls waiting for the user.
    approvals: Vec<permissions::Approval>,
    /// Calls the review model is deciding on (they wait, not run).
    reviewing: HashSet<String>,
    /// Questions of `ask` calls waiting for the user.
    questions: Vec<questions::QuestionCard>,
    /// Whether the task list above the composer shows every item.
    todos_open: bool,
    /// Task updates the user hid from the composer.
    dismissed_todo_calls: HashSet<String>,
    /// The sent message being edited, if any.
    editing: Option<MessageEdit>,
    /// Measured height of the floating composer.
    composer_height: Rc<std::cell::Cell<f32>>,
    /// Measured width of the composer's text field.
    composer_input_width: Rc<std::cell::Cell<f32>>,
    /// The row whose text was just copied (shows a check mark briefly).
    copied_row: Option<usize>,
    /// Times the streaming response.
    response_timer: ResponseTimer,
    /// Output tokens per second of the last streamed response.
    tokens_per_second: Option<f64>,
    progress: Entity<progress::RunProgress>,
    status_task: Option<Task<()>>,
    /// The `/` or `@` completion menu.
    completion: Option<composer::Completion>,
    /// Scrolls the selected completion item into view.
    completion_scroll: ScrollHandle,
    /// The draft the user closed the menu for (not reopened until it changes).
    suppress_completion: Option<String>,
    /// Project files for `@` completion.
    /// Files and folders for `@` completion, and when they were listed.
    file_list: Option<(Arc<Vec<zp_core::attachments::PathEntry>>, Instant)>,
    loading_files: bool,
    /// Images for the next message.
    attachments: Vec<zp_core::attachments::ImageAttachment>,
    /// A note above the composer (command output, attachment problems).
    info: Option<String>,
    /// The provider a message could not be sent to for lack of an API key.
    needs_key: Option<String>,
    /// The last attempt to send found no model selected.
    needs_model: bool,
    compacting: bool,
    /// The run is summarizing older messages before its next request.
    auto_compacting: bool,
    /// What the running run has named in the temporary directories.
    temp_run: Option<temp_files::TempRun>,
    /// Temporary files finished runs created, by the row they are shown under.
    temp_uses: temp_files::TempUses,
    /// Whether the session tree overlay is shown.
    tree_open: bool,
    /// A model title was already requested for this session.
    title_requested: bool,
    focus_handle: FocusHandle,
    _agent_events: Option<zp_agent::Subscription>,
    _pump: Option<Task<()>>,
    _approvals: Option<Task<()>>,
    _questions: Option<Task<()>>,
    run_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConversationEvent> for ConversationView {}

impl ConversationView {
    /// A conversation continuing `session` (new or opened) in project `root`.
    pub fn new(
        root: PathBuf,
        session: Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let focus_handle = cx.focus_handle();
        let model_filter = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        // The composer redraws itself while typing; only the model filter changes our list.
        let subscriptions = vec![
            cx.subscribe(&model_filter, |this, _, _: &TextChanged, cx| {
                this.reset_model_selection(cx);
                cx.notify();
            }),
            // The composer redraws itself; only the completion menu depends on its text.
            cx.subscribe(&composer, |this, composer, _: &TextChanged, cx| {
                let text = composer.read(cx).as_str();
                if this
                    .history_index
                    .is_some_and(|index| this.prompt_history[index] != text)
                {
                    this.history_index = None;
                    this.history_draft.clear();
                }
                if this.suppress_completion.as_deref() == Some(text) {
                    return;
                }
                this.suppress_completion = None;
                this.update_completion(cx);
            }),
        ];
        let mut this = Self {
            text_selection: TextSelection::new(focus_handle.clone()),
            root,
            coding: None,
            unavailable: None,
            transcript: Transcript::default(),
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            running: false,
            run_error: None,
            composer,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            model_filter,
            model_selected: 0,
            model_scroll: ScrollHandle::new(),
            picker: None,
            review_model_picker: None,
            review_model_subscription: None,
            picker_closed_at: None,
            thinking_expanded: HashMap::new(),
            tool_expanded: HashMap::new(),
            held_open: RefCell::new(HashMap::new()),
            disclosures: disclosure::Disclosures::default(),
            #[cfg(test)]
            block_collapses: 0,
            display: crate::store::AppSettings::transcript(cx),
            markdown_cache: RefCell::default(),
            tool_diffs: RefCell::default(),
            turn_changes: RefCell::default(),
            turn_files_expanded: HashSet::new(),
            full_subagent_results: HashSet::new(),
            change_log_cache: RefCell::default(),
            approvals: Vec::new(),
            reviewing: HashSet::new(),
            questions: Vec::new(),
            todos_open: false,
            dismissed_todo_calls: HashSet::new(),
            editing: None,
            composer_height: Rc::default(),
            composer_input_width: Rc::default(),
            copied_row: None,
            response_timer: ResponseTimer::default(),
            tokens_per_second: None,
            progress: cx.new(|_| progress::RunProgress::default()),
            status_task: None,
            completion: None,
            completion_scroll: ScrollHandle::new(),
            suppress_completion: None,
            file_list: None,
            loading_files: false,
            attachments: Vec::new(),
            info: None,
            needs_key: None,
            needs_model: false,
            compacting: false,
            auto_compacting: false,
            temp_run: None,
            temp_uses: temp_files::TempUses::default(),
            tree_open: false,
            title_requested: false,
            focus_handle,
            _agent_events: None,
            _pump: None,
            _approvals: None,
            _questions: None,
            run_task: None,
            _subscriptions: subscriptions,
        };
        match this.start_session(session, cx) {
            Ok(()) => {}
            Err(error) => this.unavailable = Some(format!("{error:#}")),
        }
        this.prompt_history = this
            .transcript
            .rows()
            .iter()
            .filter_map(|row| match row {
                transcript::Row::User { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect();
        this.focus_composer(window, cx);
        this
    }

    fn start_session(&mut self, session: Session, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let models = cx
            .try_global::<AppModels>()
            .map(|m| m.models.clone())
            .ok_or_else(|| anyhow::anyhow!("models are not initialized"))?;
        // New conversations prefer the last usable model; without any usable provider,
        // the user picks a model before sending.
        let new = session_model(&models, &session).is_none();
        let model = new.then(|| AppModels::default_model(cx)).flatten();
        let mut options = CodingSessionOptions::new(models, &self.root, session);
        if new {
            // A new conversation: start at the default reasoning level.
            options.thinking_level = crate::store::AppSettings::get(cx).default_thinking;
        }
        options.model = model;
        options.global_context_dir = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone());
        options.blobs = AppPaths::blobs(cx);
        options.trusted = crate::approvals::is_trusted(&self.root, cx);
        options.resources = crate::mcp::project_resources(&self.root, cx);
        options.compaction = crate::store::AppSettings::get(cx)
            .compaction
            .unwrap_or_default();
        options.request_options.cache_retention = crate::store::AppSettings::get(cx)
            .cache_retention
            .unwrap_or_default();
        options.tools = Some(crate::mcp::project_tools(&self.root, cx));
        options.subagents = crate::approvals::project_subagents(&self.root, cx);
        let (approval_sender, mut approval_receiver) = futures::channel::mpsc::unbounded();
        options.permissions = Some(Arc::new(zp_core::permissions::PermissionGate::new(
            crate::approvals::project_policy(&self.root, cx),
            Arc::new(crate::approvals::ChannelApprover::new(approval_sender)),
            options.models.clone(),
        )));
        let (question_sender, mut question_receiver) = futures::channel::mpsc::unbounded();
        options.asker = Some(Arc::new(crate::approvals::ChannelAsker::new(
            question_sender,
        )));
        options.response_elapsed_seconds =
            Some(Arc::new(self.progress.read(cx).elapsed_sampler(cx)));
        let coding = Arc::new(CodingSession::new(options)?);
        let approvals = cx.spawn(async move |this, cx| {
            while let Some(event) = approval_receiver.next().await {
                if this
                    .update(cx, |this, cx| match event {
                        crate::approvals::GateEvent::Approval(pending) => {
                            this.add_approval(*pending, cx)
                        }
                        crate::approvals::GateEvent::Reviewing { call_id, active } => {
                            this.set_reviewing(call_id, active, cx)
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self._approvals = Some(approvals);
        let questions = cx.spawn(async move |this, cx| {
            while let Some(pending) = question_receiver.next().await {
                if this
                    .update(cx, |this, cx| this.add_questions(pending, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self._questions = Some(questions);

        // Events carry their arrival time: batches are applied up to a frame later.
        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<ReceivedEvent>();
        let session_owner = Arc::downgrade(&coding);
        let subscription = coding.agent().subscribe(move |event| {
            let mut received = ReceivedEvent::new(Instant::now(), event.clone());
            if let AgentEvent::MessageEnd { message } = event
                && matches!(
                    message.as_llm(),
                    Some(zp_ai::Message::User(_) | zp_ai::Message::Assistant(_))
                )
                && let Some(coding) = session_owner.upgrade()
            {
                // The persistence listener has already appended this message.
                let session = coding.session();
                received.timing = session
                    .entries()
                    .iter()
                    .rev()
                    .find(|entry| entry.message().is_some())
                    .filter(|entry| entry.message() == Some(message))
                    .and_then(MessageTiming::from_entry);
            }
            let _ = sender.unbounded_send(received);
        });
        let mut awaiting_first_save = {
            let session = coding.session();
            session.file().is_some() && !session.is_persisted()
        };
        let pump = cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.next().await {
                let mut batch = vec![first];
                while let Ok(event) = receiver.try_recv() {
                    batch.push(event);
                }
                let batch = coalesce_tool_updates(batch);
                if this
                    .update(cx, |this, cx| {
                        // The coding session saves each completed message before forwarding it.
                        if awaiting_first_save
                            && batch.iter().any(|received| {
                                matches!(received.event, AgentEvent::MessageEnd { .. })
                            })
                            && this.session_file().is_some()
                        {
                            awaiting_first_save = false;
                            cx.emit(ConversationEvent::SessionCreated);
                        }
                        this.apply_events(batch, cx);
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(FRAME_INTERVAL).await;
            }
        });

        self.text_selection.clear();
        self.turn_changes.borrow_mut().clear();
        self.turn_files_expanded.clear();
        self.change_log_cache.borrow_mut().take();
        self.disclosures.clear();
        self.transcript = Transcript::from_session(&coding.session(), coding.context_tokens());
        // One more item at the end: room for the floating composer.
        self.list_state = ListState::new(self.transcript.len() + 1, ListAlignment::Top, px(1000.));
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        self.coding = Some(coding);
        self._agent_events = Some(subscription);
        self._pump = Some(pump);
        Ok(())
    }

    fn apply_events(&mut self, events: Vec<ReceivedEvent>, cx: &mut Context<Self>) {
        let mut old_len = self.transcript.len();
        let mut changes = Changes::default();
        let mut compacted = false;
        for ReceivedEvent { at, event, timing } in &events {
            self.track_temp_paths(event);
            if let AgentEvent::ContextCompacting { .. } = event {
                self.auto_compacting = true;
                continue;
            }
            if let AgentEvent::ContextCompacted { compaction, .. } = event {
                self.auto_compacting = false;
                self.reload_transcript_at(Some(&compaction.entry_id), compaction.tokens_after);
                self.info = Some(format!(
                    "Compacted the conversation: about {} → {} tokens",
                    crate::ui::format_tokens(compaction.tokens_before),
                    crate::ui::format_tokens(compaction.tokens_after),
                ));
                old_len = self.transcript.len();
                changes = Changes {
                    usage: true,
                    ..Changes::default()
                };
                compacted = true;
                continue;
            }
            if let AgentEvent::ContextCompactionFailed { error } = event {
                self.auto_compacting = false;
                self.info = Some(error.clone());
            }
            if self.time_response(*at, event) {
                changes.usage = true;
            }
            self.transcript.apply(event, *timing, &mut changes);
        }
        for &row in changes.dirty.iter().filter(|&&row| row < old_len) {
            self.list_state.remeasure_items(row..row + 1);
        }
        let new_len = self.transcript.len();
        if new_len > old_len {
            // The previous last row loses the working indicator.
            if old_len > 0 && !changes.dirty.contains(&(old_len - 1)) {
                self.list_state.remeasure_items(old_len - 1..old_len);
            }
            self.list_state.splice(old_len..old_len, new_len - old_len);
        }
        if changes.usage {
            cx.emit(ConversationEvent::StatusChanged);
        }
        let files_changed = events.iter().any(|received| {
            matches!(&received.event, AgentEvent::ToolExecutionEnd { result, .. } if result.details.is_some())
        });
        if compacted || files_changed {
            self.turn_changes.borrow_mut().clear();
            self.change_log_cache.borrow_mut().take();
            cx.emit(ConversationEvent::FilesChanged);
        }
        self.schedule_collapses(cx);
        if changes.finished && self.running {
            self.finish_run(cx);
        }
        cx.notify();
    }

    /// Whether a held block is still in progress: a running (or still being written)
    /// tool call, or a thinking block that still receives text.
    pub(crate) fn block_in_progress(&self, block: &HeldBlock) -> bool {
        match block {
            HeldBlock::Tool(id) => match self.transcript.tool(id).map(|state| state.status) {
                Some(transcript::ToolStatus::Running) => true,
                Some(_) => false,
                None => self.transcript.streaming_row().is_some_and(|row| {
                    self.transcript
                        .assistant(row)
                        .is_some_and(|message| message.tool_calls().any(|call| &call.id == id))
                }),
            },
            HeldBlock::Thinking { row, block } => self.transcript.thinking_is_running(*row, *block),
        }
    }

    /// The row a held block is drawn in.
    fn held_row(&self, block: &HeldBlock) -> Option<usize> {
        match block {
            HeldBlock::Tool(id) => self.transcript.row_of_tool(id).or_else(|| {
                self.transcript.streaming_row().filter(|&row| {
                    self.transcript
                        .assistant(row)
                        .is_some_and(|message| message.tool_calls().any(|call| &call.id == id))
                })
            }),
            HeldBlock::Thinking { row, .. } => Some(*row),
        }
    }

    /// Redraws each held block that is no longer in progress once it has been open for
    /// [`MIN_EXPANDED`] (rendering keeps it open until then), so it collapses on time.
    fn schedule_collapses(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let done: Vec<(HeldBlock, Duration)> = {
            let mut held = self.held_open.borrow_mut();
            held.iter_mut()
                .filter(|(block, held)| !held.collapsing && !self.block_in_progress(block))
                .filter_map(|(block, held)| {
                    held.collapsing = true;
                    let remaining = (held.since + MIN_EXPANDED).checked_duration_since(now)?;
                    (!remaining.is_zero()).then(|| (block.clone(), remaining))
                })
                .collect()
        };
        for (block, remaining) in done {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(remaining).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(row) = this.held_row(&block) {
                        this.list_state.remeasure_items(row..row + 1);
                        #[cfg(test)]
                        {
                            this.block_collapses += 1;
                        }
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    /// Tracks the streaming response; returns whether the output speed was updated.
    fn time_response(&mut self, at: Instant, event: &AgentEvent) -> bool {
        use zp_ai::{AssistantMessageEvent, Message, StopReason};
        match event {
            AgentEvent::MessageStart {
                message: zp_agent::AgentMessage::Llm(Message::Assistant(_)),
            } => {
                self.response_timer = ResponseTimer {
                    started: Some(at),
                    ..ResponseTimer::default()
                };
                self.tokens_per_second = None;
                true
            }
            AgentEvent::MessageUpdate { event } => {
                if let AssistantMessageEvent::Usage { usage } = event
                    && usage.output > 0
                {
                    self.response_timer.reported_output = usage.output;
                    self.response_timer.reported_bytes = self.response_timer.output_bytes;
                }
                if let AssistantMessageEvent::TextDelta { delta, .. }
                | AssistantMessageEvent::ThinkingDelta { delta, .. }
                | AssistantMessageEvent::ToolCallDelta { delta, .. } = event
                    && !delta.is_empty()
                {
                    self.response_timer.first_output.get_or_insert(at);
                    self.response_timer.output_bytes += delta.len() as u64;
                }
                false
            }
            AgentEvent::MessageEnd {
                message: zp_agent::AgentMessage::Llm(Message::Assistant(message)),
            } => {
                let timer = std::mem::take(&mut self.response_timer);
                self.tokens_per_second =
                    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                        None
                    } else {
                        timer.speed(at, message.usage.output)
                    };
                true
            }
            AgentEvent::AgentEnd { .. } => {
                self.response_timer = ResponseTimer::default();
                false
            }
            _ => false,
        }
    }

    /// Shows a check mark on row `ix`'s copy button for a moment.
    pub(super) fn show_copied(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.copied_row = Some(ix);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.copied_row == Some(ix) {
                    this.copied_row = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Whether the agent is working (running or compacting).
    pub(crate) fn is_busy(&self) -> bool {
        self.running || self.compacting
    }

    /// Remeasures the space kept for the composer after the last row.
    pub(super) fn refresh_spacer(&mut self) {
        let len = self.transcript.len();
        self.list_state.remeasure_items(len..len + 1);
    }

    /// Remeasures the last row, which shows the working indicator while busy.
    pub(super) fn refresh_last_row(&mut self) {
        let len = self.transcript.len();
        if len > 0 {
            self.list_state.remeasure_items(len - 1..len);
        }
    }

    /// Rebuilds the rows from the agent's transcript (after retry or branch changes).
    fn reload_transcript(&mut self) {
        let tokens = self
            .coding
            .as_ref()
            .map_or(0, |coding| coding.context_tokens());
        self.reload_transcript_at(None, tokens);
    }

    /// Rebuilds the rows up to entry `leaf` (the latest when `None`), with the context
    /// measured at `context_tokens`.
    fn reload_transcript_at(&mut self, leaf: Option<&str>, context_tokens: u64) {
        self.text_selection.clear();
        self.turn_changes.borrow_mut().clear();
        self.turn_files_expanded.clear();
        self.change_log_cache.borrow_mut().take();
        // Rows change meaning; the cards would sit under the wrong ones.
        self.temp_uses.clear();
        // Choices kept by row would apply to other blocks; tool diffs are recomputed.
        self.thinking_expanded.clear();
        self.tool_expanded
            .retain(|key, _| !key.starts_with(render::COMPACTION_KEY_PREFIX));
        self.tool_diffs.borrow_mut().clear();
        if let Some(coding) = &self.coding {
            self.transcript = Transcript::from_session_at(&coding.session(), leaf, context_tokens);
            self.editing = None;
            self.held_open.borrow_mut().clear();
            self.disclosures.clear();
            self.list_state.reset(self.transcript.len() + 1);
            self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        }
    }

    // ----- accessors -----------------------------------------------------------------

    /// The session file, once it has been written.
    pub fn session_file(&self) -> Option<PathBuf> {
        let coding = self.coding.as_ref()?;
        let session = coding.session();
        session
            .is_persisted()
            .then(|| session.file().map(Path::to_path_buf))
            .flatten()
    }

    /// A tab title: the session name or first message.
    pub fn title(&self) -> String {
        let named = self
            .coding
            .as_ref()
            .and_then(|coding| coding.session().name().map(str::to_owned));
        if let Some(name) = named {
            return name;
        }
        // The first message of the branch (compacted ones are no longer in the transcript).
        let first = self.coding.as_ref().and_then(|coding| {
            coding
                .session()
                .branch_path(None)
                .iter()
                .find_map(|entry| match entry.message() {
                    Some(zp_agent::AgentMessage::Llm(zp_ai::Message::User(user))) => Some(
                        user.content
                            .iter()
                            .filter_map(zp_ai::UserContent::as_text)
                            .collect::<String>(),
                    ),
                    _ => None,
                })
        });
        if let Some(first) = first {
            let (_, typed) = zp_core::attachments::split_file_blocks(&first);
            if !typed.trim().is_empty() {
                return zp_tools::shorten(typed.trim(), 32);
            }
        }
        self.transcript
            .rows()
            .iter()
            .find_map(|row| match row {
                transcript::Row::User { text, .. } => Some(zp_tools::shorten(text, 32)),
                _ => None,
            })
            .unwrap_or_else(|| "New Chat".to_owned())
    }

    /// Restores or replaces the composer draft.
    pub fn set_draft(&self, text: &str, cx: &mut App) {
        self.composer
            .update(cx, |composer, cx| composer.emplace(text, cx));
    }

    /// The composer input (tests).
    #[cfg(test)]
    pub fn composer_input(&self) -> Entity<EditableTextState> {
        self.composer.clone()
    }

    /// The current composer draft.
    pub fn draft(&self, cx: &App) -> String {
        self.composer.read(cx).as_str().to_owned()
    }

    /// The session tree as displayed.
    pub fn tree_rows(&self) -> Vec<tree::TreeRow> {
        let Some(coding) = &self.coding else {
            return Vec::new();
        };
        let session = coding.session();
        let active: HashSet<String> = session
            .branch_path(None)
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        tree::flatten(&session.tree(), &active, session.leaf_id())
    }

    #[cfg(test)]
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The transcript's scroll position and whether it follows new output (tests).
    #[cfg(test)]
    pub fn transcript_scroll(&self) -> (gpui::ListOffset, bool) {
        (
            self.list_state.logical_scroll_top(),
            self.list_state.is_following_tail(),
        )
    }

    /// Shared text selection for native interaction screenshots.
    #[cfg(feature = "visual-tests")]
    pub fn text_selection(&self) -> TextSelection {
        self.text_selection.clone()
    }

    /// Whether every tool call has finished, and the number of rows (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn transcript_tool_done(&self) -> bool {
        (0..self.transcript.len()).any(|row| {
            self.transcript.assistant(row).is_some_and(|message| {
                message.tool_calls().any(|call| {
                    self.transcript
                        .tool(&call.id)
                        .is_some_and(|state| state.status == transcript::ToolStatus::Done)
                })
            })
        })
    }

    #[cfg(feature = "visual-tests")]
    pub fn transcript_rows(&self) -> usize {
        self.transcript.len()
    }

    /// Feeds scripted agent events, as if a run were in progress (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn inject(&mut self, events: Vec<AgentEvent>, running: bool, cx: &mut Context<Self>) {
        self.running = running;
        if running {
            self.start_status_updates(cx);
        } else {
            self.stop_status_updates(cx);
        }
        self.refresh_last_row();
        let now = Instant::now();
        self.apply_events(
            events
                .into_iter()
                .map(|event| ReceivedEvent::new(now, event))
                .collect(),
            cx,
        );
    }

    /// Scrolls the transcript so that `row` is at the top (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn scroll_to_row(&mut self, row: usize) {
        self.list_state.set_follow_mode(gpui::FollowMode::Normal);
        self.list_state.scroll_to(gpui::ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        });
    }

    /// The provider whose key is missing for the last attempt to send, while it still is
    /// (tests and rendering).
    pub fn needs_key(&self, cx: &App) -> Option<String> {
        self.needs_key
            .clone()
            .filter(|provider| !AppModels::has_credentials(provider, cx))
    }

    /// Whether the last attempt to send found no model, while there still is none.
    pub fn needs_model(&self) -> bool {
        self.needs_model && self.model().is_none()
    }

    /// Re-reads the project's prompt templates and skills, and its tools (MCP servers
    /// connected or gone).
    pub fn refresh_resources(&mut self, cx: &mut Context<Self>) {
        if let Some(coding) = &self.coding {
            coding.set_resources(crate::mcp::project_resources(&self.root, cx));
            if !self.running {
                coding.set_tools(crate::mcp::project_tools(&self.root, cx));
            }
        }
        self.update_completion(cx);
    }

    /// Updates the tools after MCP servers changed (not during a run).
    pub fn refresh_tools(&mut self, cx: &mut Context<Self>) {
        if let Some(coding) = &self.coding
            && !self.running
        {
            coding.set_tools(crate::mcp::project_tools(&self.root, cx));
        }
    }

    /// Names of the agent's tools (tests).
    #[cfg(all(test, unix))]
    pub fn tool_names(&self) -> Vec<String> {
        self.coding
            .as_ref()
            .map(|c| {
                c.agent()
                    .tools()
                    .iter()
                    .map(|t| t.name().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Stops the running agent, if any.
    pub fn abort(&mut self, cx: &mut Context<Self>) {
        if let Some(coding) = &self.coding {
            coding.agent().abort();
        }
        cx.notify();
    }

    /// Cancels the active run or compaction as the project is unloaded.
    pub(crate) fn stop_for_folder_switch(&mut self, cx: &mut Context<Self>) {
        self.abort(cx);
        self.running = false;
        self.compacting = false;
        self.auto_compacting = false;
        self.run_task = None;
        self.stop_status_updates(cx);
    }

    /// The error of the last run, if it failed.
    pub fn failure(&self) -> Option<String> {
        self.run_error.clone().or_else(|| {
            let row = self.transcript.failed_response()?;
            self.transcript
                .assistant(row)
                .and_then(|m| m.error_message.clone())
        })
    }

    /// Whether the last response failed.
    pub fn has_failed(&self) -> bool {
        self.run_error.is_some() || self.transcript.failed_response().is_some()
    }

    /// Whether nothing was said yet.
    pub fn is_empty(&self) -> bool {
        self.transcript.is_empty()
    }

    /// The shown branch's leaf entry, to restore it later.
    pub fn leaf_id(&self) -> Option<String> {
        let coding = self.coding.as_ref()?;
        coding.session().leaf_id().map(str::to_owned)
    }

    /// Shows the branch ending at `leaf` (when restoring a tab).
    pub fn restore_leaf(&mut self, leaf: &str, recover_restores: bool, cx: &mut Context<Self>) {
        let Some(coding) = &self.coding else {
            return;
        };
        let leaf = {
            let session = coding.session();
            if session.entry(leaf).is_none() {
                return;
            }
            let mut leaf = leaf.to_owned();
            // Older tab state stopped before restores appended to the session file.
            if recover_restores {
                loop {
                    let children = session.children(&leaf);
                    let [next] = children.as_slice() else {
                        break;
                    };
                    if !matches!(next.message(), Some(zp_agent::AgentMessage::Custom(message))
                        if message.custom_type == zp_core::changes::FILE_RESTORE_TYPE)
                    {
                        break;
                    }
                    leaf = next.id.clone();
                }
            }
            leaf
        };
        if coding.branch(&leaf).is_ok() {
            self.reload_transcript();
            cx.notify();
        }
    }

    /// Names the session.
    pub fn rename(&mut self, name: Option<String>, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let coding = self
            .coding
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this conversation has no session"))?;
        coding.set_name(name)?;
        cx.emit(ConversationEvent::TitleChanged);
        cx.notify();
        Ok(())
    }

    /// A fork of the shown branch (for "Duplicate").
    pub fn fork_current(&self, cx: &App) -> anyhow::Result<Session> {
        let coding = self
            .coding
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this conversation has no session"))?;
        let leaf = coding
            .session()
            .leaf_id()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("the conversation is empty"))?;
        fork_session(coding, &leaf, cx)
    }

    /// The session entry of transcript `row`.
    fn entry_for_row(&self, row: usize) -> Option<String> {
        let coding = self.coding.as_ref()?;
        let ordinal = self.transcript.rows().get(row)?.ordinal();
        coding.context_entry_ids().get(ordinal).cloned()
    }

    fn report(&mut self, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        if let Err(error) = result {
            self.run_error = Some(format!("{error:#}"));
        }
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    /// Continues from `row`: later messages move to an abandoned branch.
    pub(crate) fn branch_from(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entry_for_row(row) else {
            return;
        };
        let target = Some(entry.clone());
        self.change_branch(
            target,
            Box::new(move |this, _, cx| {
                let Some(coding) = this.coding.clone() else {
                    return;
                };
                let result = coding.branch(&entry);
                this.after_branch_change(result, cx);
            }),
            window,
            cx,
        );
    }

    /// Edits user message `row` in place. Nothing changes until the edit is sent.
    pub(crate) fn edit_and_resend(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_busy() {
            return;
        }
        let Some(transcript::Row::User { text, .. }) = self.transcript.rows().get(row).cloned()
        else {
            return;
        };
        if let Some(previous) = self.editing.take() {
            self.list_state
                .remeasure_items(previous.row..previous.row + 1);
        }
        let input = cx.new(|cx| EditableTextState::new(StringStorage::from(text.clone()), cx));
        input.update(cx, |input, cx| input.move_to(text.len(), cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        // The field may grow or shrink the row.
        let changed = cx.subscribe(&input, move |this, _, _: &TextChanged, cx| {
            if this.editing.as_ref().is_some_and(|edit| edit.row == row) {
                this.list_state.remeasure_items(row..row + 1);
                cx.notify();
            }
        });
        self.editing = Some(MessageEdit {
            row,
            input,
            _changed: changed,
        });
        self.list_state.remeasure_items(row..row + 1);
        cx.notify();
    }

    /// The message being edited (rendering and tests).
    pub(crate) fn editing(&self) -> Option<&MessageEdit> {
        self.editing.as_ref()
    }

    /// Stops editing without changing anything.
    pub(crate) fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.editing.take() {
            self.list_state.remeasure_items(edit.row..edit.row + 1);
            self.focus_composer(window, cx);
            cx.notify();
        }
    }

    /// Sends the edited message: the conversation continues from before the original
    /// message on a new branch (the original stays in the session tree).
    pub(crate) fn submit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.editing.as_ref() else {
            return;
        };
        let text = edit.input.read(cx).as_str().trim().to_owned();
        if text.is_empty() || self.is_busy() {
            return;
        }
        let row = edit.row;
        let (Some(coding), Some(entry)) = (self.coding.clone(), self.entry_for_row(row)) else {
            return;
        };
        let parent = coding
            .session()
            .entry(&entry)
            .and_then(|entry| entry.parent_id.clone());
        self.change_branch(
            parent,
            Box::new(move |this, window, cx| {
                let result = coding.branch_before(&entry);
                let ok = result.is_ok();
                this.after_branch_change(result, cx);
                if ok {
                    this.send_text(&text, window, cx);
                }
            }),
            window,
            cx,
        );
    }

    /// Sends `text` as if typed in the composer, keeping the composer's own draft.
    fn send_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.composer.read(cx).as_str().to_owned();
        let attachments = std::mem::take(&mut self.attachments);
        self.composer
            .update(cx, |composer, cx| composer.emplace(text, cx));
        self.send(&SendMessage, window, cx);
        if self.running {
            self.composer
                .update(cx, |composer, cx| composer.emplace(&draft, cx));
            self.attachments = attachments;
        } else {
            // Not sent (e.g. a key is missing): the text waits in the composer.
            self.attachments.extend(attachments);
        }
        self.completion = None;
    }

    fn after_branch_change(&mut self, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        self.reload_transcript();
        cx.emit(ConversationEvent::Activity);
        cx.emit(ConversationEvent::FilesChanged);
        self.report(result, cx);
    }

    /// Copies the conversation up to `row` into a new session for a new tab.
    pub(crate) fn fork_from(&mut self, row: usize, cx: &mut Context<Self>) {
        let (Some(coding), Some(entry)) = (self.coding.clone(), self.entry_for_row(row)) else {
            return;
        };
        match fork_session(&coding, &entry, cx) {
            Ok(session) => cx.emit(ConversationEvent::Forked(Rc::new(RefCell::new(Some(
                session,
            ))))),
            Err(error) => self.report(Err(error), cx),
        }
    }

    /// Shows or hides the session tree.
    pub fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.tree_open = !self.tree_open;
        cx.notify();
    }

    pub fn is_tree_open(&self) -> bool {
        self.tree_open
    }

    /// Switches to the newest branch through entry `id` (from the tree).
    pub(crate) fn switch_branch(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        let Some(leaf) = coding.session().latest_leaf_under(id).map(str::to_owned) else {
            return;
        };
        self.change_branch(
            Some(leaf.clone()),
            Box::new(move |this, _, cx| {
                let result = coding.branch(&leaf);
                this.after_branch_change(result, cx);
            }),
            window,
            cx,
        );
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn status(&self) -> ConversationStatus {
        let agent = self.coding.as_ref().map(|coding| coding.agent());
        let model = agent.and_then(|agent| agent.model());
        let mut usage = self.transcript.usage().clone();
        let mut cache_read_rate = self.transcript.last_cache_read_rate;
        let mut output_estimated = false;
        let mut tokens_per_second = self.tokens_per_second;
        if let Some(partial) = self.transcript.streaming_usage() {
            let mut partial = partial.clone();
            let estimated = self.response_timer.estimated_output();
            if estimated > partial.output {
                partial.output = estimated;
                partial.update_total();
                output_estimated = true;
                if let Some(model) = &model {
                    model.calculate_cost(&mut partial);
                }
            }
            tokens_per_second = self
                .response_timer
                .first_output
                .and_then(|_| self.response_timer.speed(Instant::now(), partial.output));
            usage.add(&partial);
            if let Some(rate) = partial.cache_read_rate() {
                cache_read_rate = Some(rate);
                usage.last_context_tokens = partial.total_tokens;
            }
        }
        ConversationStatus {
            model,
            thinking: agent.map_or(ThinkingLevel::Off, |agent| agent.thinking_level()),
            usage,
            cache_read_rate,
            running: self.running,
            output_estimated,
            tokens_per_second,
        }
    }

    fn start_status_updates(&mut self, cx: &mut Context<Self>) {
        if self.status_task.is_some() {
            return;
        }
        self.progress.update(cx, |progress, cx| progress.start(cx));
        self.status_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STATUS_INTERVAL).await;
                if this
                    .update(cx, |this, cx| {
                        this.progress
                            .update(cx, |progress, cx| progress.refresh(cx));
                        // The progress entity redraws only when its displayed second changes.
                        // Do not invalidate transcript layout for status ticks.
                        if this.transcript.streaming_usage().is_some() {
                            cx.emit(ConversationEvent::StatusChanged);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn stop_status_updates(&mut self, cx: &mut Context<Self>) {
        self.status_task = None;
        self.progress
            .update(cx, |progress, cx| progress.set_paused(true, cx));
    }

    #[cfg(test)]
    pub(crate) fn task_elapsed(&self, cx: &App) -> Duration {
        self.progress.read(cx).elapsed(cx)
    }

    /// The selected model; `None` until the user picks one when no provider was usable.
    pub fn model(&self) -> Option<Model> {
        self.coding.as_ref()?.agent().model()
    }

    /// The reasoning levels offered: the model's, or all of them without a model.
    pub(crate) fn thinking_levels(&self) -> Vec<ThinkingLevel> {
        self.model().map_or_else(
            || ThinkingLevel::ALL.to_vec(),
            |model| model.supported_thinking_levels(),
        )
    }

    /// The requested reasoning level, clamped to the model.
    pub(crate) fn effective_thinking(&self) -> ThinkingLevel {
        let level = self
            .coding
            .as_ref()
            .map_or(ThinkingLevel::Off, |coding| coding.agent().thinking_level());
        self.model()
            .map_or(level, |model| model.clamp_thinking_level(level))
    }

    /// The focus target for this conversation: the composer, or the view itself when
    /// there is no composer.
    pub fn composer_focus(&self, cx: &App) -> FocusHandle {
        if self.coding.is_some() {
            self.composer.read(cx).focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }

    /// Focuses the composer, or the conversation itself when there is no composer.
    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        if self.coding.is_some() {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
    }

    /// Models the picker offers: those of providers with credentials (or that need none).
    fn model_choices(&self, cx: &App) -> Vec<ModelChoice> {
        let Some(app_models) = cx.try_global::<AppModels>() else {
            return Vec::new();
        };
        let models = &app_models.models;
        models
            .available_providers()
            .iter()
            .flat_map(|provider| {
                models
                    .models(Some(provider.id()))
                    .into_iter()
                    .map(|model| ModelChoice {
                        model,
                        provider_name: provider.name().to_owned(),
                    })
            })
            .collect()
    }

    /// The picker's models (tests).
    #[cfg(test)]
    pub fn model_choices_for_test(&self, cx: &App) -> Vec<ModelChoice> {
        self.model_choices(cx)
    }

    fn filtered_models(&self, cx: &App) -> Vec<ModelChoice> {
        let filter = self.model_filter.read(cx).as_str().to_owned();
        self.models_matching(&filter, cx)
    }

    /// Available models whose id or name contains every word of `filter`.
    fn models_matching(&self, filter: &str, cx: &App) -> Vec<ModelChoice> {
        let filter = filter.trim().to_lowercase();
        self.model_choices(cx)
            .into_iter()
            .filter(|choice| {
                filter.is_empty()
                    || filter.split_whitespace().all(|word| {
                        choice.model.qualified_id().to_lowercase().contains(word)
                            || choice.model.name.to_lowercase().contains(word)
                    })
            })
            .collect()
    }

    /// Parsed `text`, cached by its content. Text that is still streaming (`growing`) is
    /// parsed without caching: each frame's text is new, and caching it would only fill
    /// the cache until it is cleared, taking every other row's parse with it.
    fn parsed_markdown(&self, text: &str, growing: bool) -> Rc<Vec<Block>> {
        if growing {
            return Rc::new(parse(text));
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let key = hasher.finish();
        let mut cache = self.markdown_cache.borrow_mut();
        if let Some(blocks) = cache.get(&key) {
            return blocks.clone();
        }
        if cache.len() > 512 {
            cache.clear();
        }
        let blocks = Rc::new(parse(text));
        cache.insert(key, blocks.clone());
        blocks
    }

    // ----- actions -------------------------------------------------------------------

    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        // Enter completes, unless the draft already is the selected command.
        let raw = self.composer.read(cx).as_str().to_owned();
        let draft = raw.trim_end();
        let complete = self.completion.as_ref().is_some_and(|completion| {
            completion
                .items
                .get(completion.selected)
                .is_some_and(|item| match &item.action {
                    composer::CompletionAction::Insert { text, .. } => text.trim_end() != draft,
                    _ => true,
                })
        });
        if complete && self.accept_completion(None, window, cx) {
            return;
        }
        self.completion = None;
        let text = raw.trim().to_owned();
        if (text.is_empty() && self.attachments.is_empty()) || self.compacting {
            return;
        }
        let Some(coding) = self.coding.clone() else {
            return;
        };
        self.info = None;
        self.needs_key = None;
        self.needs_model = false;
        // `/model`, `/thinking` and `/permissions` without an argument list their
        // choices in the completion menu.
        if !self.running && self.open_command_options(&text, window, cx) {
            return;
        }
        if !self.running && self.run_builtin(&text, window, cx) {
            self.composer
                .update(cx, |composer, cx| composer.emplace("", cx));
            return;
        }
        // No model yet, or no key for its provider: keep the draft and ask for one.
        let Some(model) = coding.agent().model() else {
            self.needs_model = true;
            cx.notify();
            return;
        };
        if !AppModels::has_credentials(&model.provider, cx) {
            self.needs_key = Some(model.provider);
            cx.notify();
            return;
        }
        if !self.attachments.is_empty() && !model.supports_images() {
            self.run_error = Some(format!(
                "{} does not accept images; remove them or pick another model",
                model.qualified_id()
            ));
            cx.notify();
            return;
        }
        self.composer
            .update(cx, |composer, cx| composer.emplace("", cx));
        self.remember_prompt(&text);
        self.picker = None;
        cx.emit(ConversationEvent::Activity);
        let text = coding.expand_input(&text);
        let (message, notes) =
            zp_core::attachments::build_message(&text, &self.root, &self.attachments);
        self.attachments.clear();
        if !notes.is_empty() {
            self.info = Some(notes.join("\n"));
        }
        if self.running {
            // Typed while working: delivered after the current tool calls finish.
            coding.agent().steer(message);
            cx.notify();
            return;
        }
        self.start_run(cx, move |coding| async move {
            coding.prompt(zp_agent::PromptInput(vec![message])).await
        });
        self.focus_composer(window, cx);
    }

    fn start_run<F, Fut>(&mut self, cx: &mut Context<Self>, run: F)
    where
        F: FnOnce(Arc<CodingSession>) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<zp_agent::AgentMessage>, zp_agent::AgentError>>
            + Send
            + 'static,
    {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        coding.set_subagents(crate::approvals::project_subagents(&self.root, cx));
        coding.agent().set_cache_retention(
            crate::store::AppSettings::get(cx)
                .cache_retention
                .unwrap_or_default(),
        );
        if let Some(model) = coding.agent().model() {
            AppModels::remember_model(&model, cx);
        }
        self.running = true;
        self.start_temp_tracking();
        self.tokens_per_second = None;
        self.response_timer = ResponseTimer::default();
        self.start_status_updates(cx);
        self.run_error = None;
        self.refresh_last_row();
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        cx.emit(ConversationEvent::StatusChanged);
        let task = gpui_tokio::Tokio::spawn(cx, run(coding));
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                // A normal end arrives through the event pump (`AgentEnd`), after the last
                // streamed content; only runs that never started end here.
                let error = match result {
                    Ok(Ok(_)) => return,
                    Ok(Err(error)) => error.to_string(),
                    Err(error) => format!("the run stopped unexpectedly: {error}"),
                };
                this.run_error = Some(error);
                if this.running {
                    this.finish_run(cx);
                }
            });
        }));
        cx.notify();
    }

    /// Marks the run as over once its last event has been shown.
    fn finish_run(&mut self, cx: &mut Context<Self>) {
        self.running = false;
        self.finish_temp_tracking();
        // A compaction cancelled with the run ends without an event.
        self.auto_compacting = false;
        self.turn_changes.borrow_mut().clear();
        self.change_log_cache.borrow_mut().take();
        self.refresh_last_row();
        self.clear_approvals();
        self.clear_questions();
        self.invalidate_files();
        let failed = self.has_failed();
        self.stop_status_updates(cx);
        cx.emit(ConversationEvent::StatusChanged);
        cx.emit(ConversationEvent::RunFinished);
        if !failed {
            self.request_title(cx);
        }
        cx.notify();
    }

    /// Names an unnamed session once, with a cheap model (when enabled).
    fn request_title(&mut self, cx: &mut Context<Self>) {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        if self.title_requested || coding.session().name().is_some() {
            return;
        }
        let Some(app_models) = cx.try_global::<AppModels>().filter(|m| m.generate_titles) else {
            return;
        };
        let first = coding
            .agent()
            .messages()
            .into_iter()
            .find_map(|message| match message {
                zp_agent::AgentMessage::Llm(zp_ai::Message::User(user)) => Some(
                    user.content
                        .iter()
                        .filter_map(zp_ai::UserContent::as_text)
                        .collect::<String>(),
                ),
                _ => None,
            });
        let Some(first) = first else { return };
        self.title_requested = true;
        let models = app_models.models.clone();
        let Some(model) = coding.agent().model() else {
            return;
        };
        let model = zp_core::title::title_model(&models, &model);
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            zp_core::title::generate_title(&models, &model, &first).await
        });
        cx.spawn(async move |this, cx| {
            let title = match task.await {
                Ok(Ok(title)) => title,
                Ok(Err(error)) => {
                    tracing::debug!(%error, "could not generate a title");
                    return;
                }
                Err(_) => return,
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(coding) = &this.coding
                    && coding.session().name().is_none()
                    && coding.set_name(Some(title)).is_ok()
                {
                    cx.emit(ConversationEvent::TitleChanged);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn stop(&mut self, _: &StopRun, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            cx.notify();
            return;
        }
        match &self.coding {
            Some(coding) if self.running => coding.agent().abort(),
            _ => cx.propagate(),
        }
    }

    fn close_picker(&mut self, _: &ClosePicker, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn pick_selected_model(
        &mut self,
        _: &PickSelectedModel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choices = self.filtered_models(cx);
        let index = self.model_selected.min(choices.len().saturating_sub(1));
        if let Some(choice) = choices.into_iter().nth(index) {
            self.select_model(choice.model, window, cx);
        }
    }

    fn select_next_model(&mut self, _: &SelectNextModel, _: &mut Window, cx: &mut Context<Self>) {
        self.move_model_selection(1, cx);
    }

    fn select_previous_model(
        &mut self,
        _: &SelectPreviousModel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_model_selection(-1, cx);
    }

    /// Highlights the current model while the filter is empty, else the first match.
    fn reset_model_selection(&mut self, cx: &App) {
        self.model_selected = if self.model_filter.read(cx).as_str().trim().is_empty() {
            let current = self.model().map(|model| model.qualified_id());
            self.filtered_models(cx)
                .iter()
                .take(render::PICKER_LIMIT)
                .position(|choice| Some(choice.model.qualified_id()) == current)
                .unwrap_or(0)
        } else {
            0
        };
        self.model_scroll.scroll_to_item(self.model_selected);
    }

    /// Moves the model picker's highlight by `delta` rows, wrapping around.
    fn move_model_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.filtered_models(cx).len().min(render::PICKER_LIMIT);
        if count == 0 {
            return;
        }
        let current = self.model_selected.min(count - 1) as isize;
        self.model_selected = (current + delta).rem_euclid(count as isize) as usize;
        self.model_scroll.scroll_to_item(self.model_selected);
        cx.notify();
    }

    /// The model picker's highlighted row (tests).
    #[cfg(test)]
    pub fn model_selected_for_test(&self) -> usize {
        self.model_selected
    }

    pub(crate) fn retry(&mut self, cx: &mut Context<Self>) {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        if self.running || self.compacting {
            return;
        }
        if let Err(error) = coding.prepare_retry() {
            self.run_error = Some(format!("{error:#}"));
            cx.notify();
            return;
        }
        self.reload_transcript();
        self.start_run(
            cx,
            |coding| async move { coding.agent().continue_run().await },
        );
    }

    pub(crate) fn toggle_picker(
        &mut self,
        picker: Picker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let just_closed = self
            .picker_closed_at
            .take()
            .is_some_and(|at| at.elapsed() < Duration::from_millis(300));
        if just_closed && self.picker.is_none() {
            return;
        }
        self.picker = if self.picker == Some(picker) {
            None
        } else {
            Some(picker)
        };
        if self.picker == Some(Picker::Model) {
            self.model_filter
                .update(cx, |filter, cx| filter.emplace("", cx));
            self.reset_model_selection(cx);
            self.model_filter
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn select_model(
        &mut self,
        model: Model,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(coding) = &self.coding {
            let level = coding.agent().thinking_level();
            if let Err(error) = coding.set_model(model.clone()) {
                self.run_error = Some(format!("{error:#}"));
            } else {
                AppModels::remember_model(&model, cx);
            }
            // Keep the requested level valid for the new model.
            let clamped = model.clamp_thinking_level(level);
            if clamped != level {
                let _ = coding.set_thinking_level(clamped);
            }
        }
        self.picker = None;
        self.needs_key = None;
        self.focus_composer(window, cx);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    /// Opens Settings → Providers to sign in to `provider` or enter its key (or to add a
    /// provider).
    pub(crate) fn add_api_key(&mut self, provider: Option<String>, cx: &mut Context<Self>) {
        let Some(window) =
            crate::settings::SettingsWindow::open_at(crate::settings::Page::Providers, cx)
        else {
            return;
        };
        let _ = window.update(cx, |view, window, cx| match provider {
            Some(provider) => view.choose_provider(&provider, window, cx),
            None => view.open_add_dialog(window, cx),
        });
    }

    pub(crate) fn select_thinking(
        &mut self,
        level: ThinkingLevel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(coding) = &self.coding
            && let Err(error) = coding.set_thinking_level(level)
        {
            self.run_error = Some(format!("{error:#}"));
        }
        self.picker = None;
        self.focus_composer(window, cx);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    pub(crate) fn toggle_thinking_block(
        &mut self,
        row: usize,
        block: usize,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.thinking_expanded.insert((row, block), !expanded);
        self.list_state.remeasure_items(row..row + 1);
        cx.notify();
    }

    pub(crate) fn toggle_tool(
        &mut self,
        id: &str,
        row: usize,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.tool_expanded.insert(id.to_owned(), !expanded);
        self.list_state.remeasure_items(row..row + 1);
        cx.notify();
    }
}

/// Keeps only the last [`AgentEvent::ToolExecutionUpdate`] of each tool call in `batch`:
/// each carries the whole output so far, so earlier ones that piled up (a long command
/// while the UI stalled) need not be applied. Other events keep their order. Details of a
/// dropped update are carried into the kept one if it has none, as applying both would.
fn coalesce_tool_updates(batch: Vec<ReceivedEvent>) -> Vec<ReceivedEvent> {
    let mut kept: Vec<ReceivedEvent> = Vec::with_capacity(batch.len());
    // Index in `kept` (reversed) of the last update of each call.
    let mut last_update: HashMap<String, usize> = HashMap::new();
    for mut received in batch.into_iter().rev() {
        if let AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } = &mut received.event
        {
            if let Some(&later) = last_update.get(tool_call_id.as_str()) {
                if let AgentEvent::ToolExecutionUpdate {
                    partial_result: later,
                    ..
                } = &mut kept[later].event
                    && later.details.is_none()
                {
                    later.details = partial_result.details.take();
                }
                continue;
            }
            last_update.insert(tool_call_id.clone(), kept.len());
        }
        kept.push(received);
    }
    kept.reverse();
    kept
}

/// Copies the branch of `coding` ending at `entry` into a new session (saved next to the
/// original when there is a data directory).
fn fork_session(coding: &CodingSession, entry: &str, cx: &App) -> anyhow::Result<Session> {
    let session = coding.session();
    match AppPaths::sessions_dir(cx) {
        Some(dir) => zp_session::SessionManager::new(dir).fork(&session, entry),
        None => session.fork(entry, None),
    }
}

impl Focusable for ConversationView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use zp_agent::AgentToolResult;
    use zp_ai::{AssistantMessage, AssistantMessageEvent, FauxProvider, ToolCall, Usage};
    use zp_core::settings::{BlockDisplay, TranscriptSettings};

    use crate::store::AppSettings;

    #[gpui::test]
    fn transcript_visibility_follows_settings_and_manual_choices(cx: &mut TestAppContext) {
        let faux = FauxProvider::default();
        let _data = crate::tests_conversation::setup_models(cx, &[&faux], false);
        let (_dir, project) = crate::tests::project_dir();
        let window = crate::tests_conversation::open(cx, &project);
        let conversation = crate::tests_conversation::active(window, cx);
        let apply = |view: &mut ConversationView, event, cx: &mut Context<ConversationView>| {
            view.apply_events(vec![ReceivedEvent::new(Instant::now(), event)], cx);
        };
        let calls = ["write", "edit", "bash", "read", "custom-tool"].map(|name| ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        });
        cx.update(|cx| {
            AppSettings::update(cx, |settings| {
                settings.transcript = TranscriptSettings {
                    thinking: Some(BlockDisplay::WhileRunning),
                    file_edits: Some(BlockDisplay::WhileRunning),
                    commands: Some(BlockDisplay::WhileRunning),
                    other_tools: Some(BlockDisplay::WhileRunning),
                };
            })
            .unwrap();
        });
        conversation.update(cx, |view, cx| {
            let message = AssistantMessage::empty("faux", "faux", "faux-1");
            apply(
                view,
                AgentEvent::MessageStart {
                    message: message.into(),
                },
                cx,
            );
            apply(
                view,
                AgentEvent::MessageUpdate {
                    event: AssistantMessageEvent::ThinkingStart {
                        index: 0,
                        redacted: false,
                    },
                },
                cx,
            );
            assert!(view.thinking_is_expanded(0, 0, cx));
            apply(
                view,
                AgentEvent::MessageUpdate {
                    event: AssistantMessageEvent::ThinkingEnd {
                        index: 0,
                        signature: None,
                    },
                },
                cx,
            );
            assert!(
                view.thinking_is_expanded(0, 0, cx),
                "a block that opened while thinking stays open a moment"
            );
            assert!(view.transcript.is_streaming(0));
        });
        cx.executor().advance_clock(MIN_EXPANDED);
        cx.run_until_parked();
        conversation.update(cx, |view, cx| {
            assert!(!view.thinking_is_expanded(0, 0, cx));
            assert_eq!(view.block_collapses, 1, "redrawn when the hold ends");
            view.toggle_thinking_block(0, 0, false, cx);
            assert!(view.thinking_is_expanded(0, 0, cx));
            // The finished response, completed with the streamed calls below.
            let mut message = AssistantMessage::empty("faux", "faux", "faux-1");

            for (index, call) in calls.iter().enumerate() {
                apply(
                    view,
                    AgentEvent::MessageUpdate {
                        event: AssistantMessageEvent::ToolCallStart {
                            index: index + 1,
                            id: call.id.clone(),
                            name: call.name.clone(),
                        },
                    },
                    cx,
                );
                apply(
                    view,
                    AgentEvent::MessageUpdate {
                        event: AssistantMessageEvent::ToolCallEnd {
                            index: index + 1,
                            tool_call: call.clone(),
                        },
                    },
                    cx,
                );
                assert!(view.tool_is_expanded(0, call, false, cx));
            }
            message.content = view.transcript.assistant(0).unwrap().content.clone();
            apply(
                view,
                AgentEvent::MessageEnd {
                    message: message.into(),
                },
                cx,
            );
            for call in &calls {
                apply(
                    view,
                    AgentEvent::ToolExecutionStart {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        args: call.arguments.clone(),
                    },
                    cx,
                );
                assert!(view.tool_is_expanded(0, call, false, cx));
                if call.name == "edit" {
                    view.toggle_tool(&call.id, 0, true, cx);
                    assert!(!view.tool_is_expanded(0, call, false, cx));
                    view.toggle_tool(&call.id, 0, false, cx);
                }
                apply(
                    view,
                    AgentEvent::ToolExecutionEnd {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        result: AgentToolResult::text("done"),
                        is_error: false,
                        duration_ms: Some(1),
                    },
                    cx,
                );
                assert!(
                    view.tool_is_expanded(0, call, false, cx),
                    "a card that opened while running stays open a moment"
                );
            }
        });
        // Then the cards collapse (apart from the one the user opened), redrawn on time.
        cx.run_until_parked();
        let collapses = conversation.read_with(cx, |view, _| view.block_collapses);
        cx.executor().advance_clock(MIN_EXPANDED);
        cx.run_until_parked();
        assert_eq!(
            conversation.read_with(cx, |view, _| view.block_collapses),
            collapses + calls.len(),
            "each held card is redrawn when its hold ends"
        );
        conversation.update(cx, |view, cx| {
            for call in &calls {
                assert_eq!(
                    view.tool_is_expanded(0, call, false, cx),
                    call.name == "edit"
                );
            }
        });
        cx.update(|cx| {
            AppSettings::update(cx, |settings| {
                settings.transcript = TranscriptSettings {
                    thinking: Some(BlockDisplay::Collapsed),
                    file_edits: Some(BlockDisplay::Collapsed),
                    commands: Some(BlockDisplay::Expanded),
                    other_tools: Some(BlockDisplay::Expanded),
                };
            })
            .unwrap();
        });
        conversation.update(cx, |view, cx| {
            assert!(
                view.thinking_is_expanded(0, 0, cx),
                "manual choice survives settings changes"
            );
            for call in &calls {
                assert_eq!(
                    view.tool_is_expanded(0, call, false, cx),
                    call.name != "write"
                );
            }
            let failed = &calls[0];
            apply(
                view,
                AgentEvent::ToolExecutionEnd {
                    tool_call_id: failed.id.clone(),
                    tool_name: failed.name.clone(),
                    result: AgentToolResult::error("failed"),
                    is_error: true,
                    duration_ms: Some(1),
                },
                cx,
            );
            assert!(
                view.tool_is_expanded(0, failed, false, cx),
                "errors default to expanded"
            );
            view.toggle_tool(&failed.id, 0, true, cx);
            assert!(!view.tool_is_expanded(0, failed, false, cx));
            view.toggle_thinking_block(0, 0, true, cx);
        });
        cx.update(|cx| {
            AppSettings::update(cx, |settings| {
                settings.transcript.thinking = Some(BlockDisplay::Expanded);
            })
            .unwrap();
        });
        conversation.update(cx, |view, cx| {
            assert!(
                !view.thinking_is_expanded(0, 0, cx),
                "manual close survives settings changes"
            );
        });
    }

    #[gpui::test]
    fn rebuilt_transcript_forgets_choices_kept_by_row(cx: &mut TestAppContext) {
        let faux = FauxProvider::default();
        let _data = crate::tests_conversation::setup_models(cx, &[&faux], false);
        let (_dir, project) = crate::tests::project_dir();
        let window = crate::tests_conversation::open(cx, &project);
        let conversation = crate::tests_conversation::active(window, cx);
        cx.update(|cx| {
            AppSettings::update(cx, |settings| {
                settings.transcript.thinking = Some(BlockDisplay::Collapsed);
            })
            .unwrap();
        });
        let answer = |thinking: &str| -> zp_agent::AgentMessage {
            let mut message = AssistantMessage::empty("faux", "faux", "faux-1");
            message
                .content
                .push(zp_ai::AssistantContent::Thinking(zp_ai::ThinkingContent {
                    thinking: thinking.into(),
                    signature: None,
                    redacted: false,
                }));
            message.into()
        };
        conversation.update(cx, |view, cx| {
            // Two branches whose second answers, each with a thinking block, are row 3.
            let coding = view.coding.clone().unwrap();
            let (first, second) = {
                let mut session = coding.session();
                session
                    .append_message(zp_agent::AgentMessage::user("one"))
                    .unwrap();
                let answered = session.append_message(answer("a")).unwrap();
                session
                    .append_message(zp_agent::AgentMessage::user("two"))
                    .unwrap();
                let first = session.append_message(answer("b")).unwrap();
                session.branch(&answered).unwrap();
                session
                    .append_message(zp_agent::AgentMessage::user("two, again"))
                    .unwrap();
                (first, session.append_message(answer("c")).unwrap())
            };
            coding.branch(&first).unwrap();
            view.reload_transcript();
            assert_eq!(view.transcript.len(), 4);
            view.toggle_thinking_block(3, 0, false, cx);
            view.toggle_tool("compaction-1", 1, false, cx);
            view.toggle_tool("call-1", 3, false, cx);
            view.tool_diffs
                .borrow_mut()
                .insert("call-1".into(), Rc::new(Err("old".into())));
            assert!(view.thinking_is_expanded(3, 0, cx));

            coding.branch(&second).unwrap();
            view.reload_transcript();
            assert_eq!(view.transcript.len(), 4);
            assert!(
                !view.thinking_is_expanded(3, 0, cx),
                "another answer's thinking is now row 3"
            );
            assert!(!view.tool_expanded.contains_key("compaction-1"));
            assert_eq!(
                view.tool_expanded.get("call-1"),
                Some(&true),
                "tool cards are kept by call id"
            );
            assert!(view.tool_diffs.borrow().is_empty());
        });
    }

    fn tool_update(id: &str, output: &str, details: Option<serde_json::Value>) -> ReceivedEvent {
        let mut partial_result = AgentToolResult::text(output);
        partial_result.details = details;
        ReceivedEvent::new(
            Instant::now(),
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                partial_result,
            },
        )
    }

    /// `(call id, output)` of updates and the kind of other events.
    fn summary(batch: &[ReceivedEvent]) -> Vec<String> {
        batch
            .iter()
            .map(|received| match &received.event {
                AgentEvent::ToolExecutionUpdate {
                    tool_call_id,
                    partial_result,
                    ..
                } => format!(
                    "{tool_call_id}: {}",
                    partial_result.content[0].as_text().unwrap()
                ),
                event => event.name().to_owned(),
            })
            .collect()
    }

    #[test]
    fn coalesce_tool_updates_keeps_the_last_snapshot_of_each_call() {
        let end = |id: &str| {
            ReceivedEvent::new(
                Instant::now(),
                AgentEvent::ToolExecutionEnd {
                    tool_call_id: id.into(),
                    tool_name: "bash".into(),
                    result: AgentToolResult::text("done"),
                    is_error: false,
                    duration_ms: None,
                },
            )
        };
        let batch = vec![
            tool_update("a", "1", Some(serde_json::json!({"step": 1}))),
            tool_update("b", "1", None),
            tool_update("a", "12", None),
            end("b"),
            tool_update("a", "123", None),
            end("a"),
        ];
        let coalesced = coalesce_tool_updates(batch);
        assert_eq!(
            summary(&coalesced),
            ["b: 1", "tool_execution_end", "a: 123", "tool_execution_end"]
        );
        let AgentEvent::ToolExecutionUpdate { partial_result, .. } = &coalesced[2].event else {
            unreachable!()
        };
        assert_eq!(
            partial_result.details,
            Some(serde_json::json!({"step": 1})),
            "the latest details are kept"
        );
    }

    #[gpui::test]
    fn live_usage_includes_cache_and_recalibrates_to_provider_counts(cx: &mut TestAppContext) {
        let faux = FauxProvider::default();
        let _data = crate::tests_conversation::setup_models(cx, &[&faux], false);
        let (_dir, project) = crate::tests::project_dir();
        let window = crate::tests_conversation::open(cx, &project);
        let conversation = crate::tests_conversation::active(window, cx);
        let at = Instant::now() - Duration::from_secs(2);
        let mut message = AssistantMessage::empty("faux", "faux", "faux-1");
        let usage = Usage {
            input: 10,
            cache_read: 90,
            output: 1,
            total_tokens: 101,
            ..Usage::default()
        };
        conversation.update(cx, |view, cx| {
            view.apply_events(
                vec![
                    ReceivedEvent::new(
                        at,
                        AgentEvent::MessageStart {
                            message: message.clone().into(),
                        },
                    ),
                    ReceivedEvent::new(
                        at,
                        AgentEvent::MessageUpdate {
                            event: AssistantMessageEvent::Usage {
                                usage: usage.clone(),
                            },
                        },
                    ),
                    ReceivedEvent::new(
                        at,
                        AgentEvent::MessageUpdate {
                            event: AssistantMessageEvent::TextStart { index: 0 },
                        },
                    ),
                    ReceivedEvent::new(
                        at,
                        AgentEvent::MessageUpdate {
                            event: AssistantMessageEvent::TextDelta {
                                index: 0,
                                delta: "x".repeat(400),
                            },
                        },
                    ),
                ],
                cx,
            );
        });
        let live = conversation.read_with(cx, |view, _| view.status());
        assert_eq!(
            (live.usage.input, live.usage.cache_read, live.usage.output),
            (10, 90, 101)
        );
        assert!(live.output_estimated);
        assert!(live.tokens_per_second.is_some_and(|speed| speed > 0.));

        message.usage = Usage {
            output: 20,
            total_tokens: 120,
            ..usage
        };
        conversation.update(cx, |view, cx| {
            view.apply_events(
                vec![ReceivedEvent::new(
                    at,
                    AgentEvent::MessageUpdate {
                        event: AssistantMessageEvent::Usage {
                            usage: message.usage.clone(),
                        },
                    },
                )],
                cx,
            );
        });
        let calibrated = conversation.read_with(cx, |view, _| view.status());
        assert_eq!(calibrated.usage.output, 20);
        assert!(!calibrated.output_estimated);
        conversation.update(cx, |view, cx| {
            view.apply_events(
                vec![ReceivedEvent::new(
                    at,
                    AgentEvent::MessageEnd {
                        message: message.into(),
                    },
                )],
                cx,
            );
        });
        let finished = conversation.read_with(cx, |view, _| view.status());
        assert_eq!(finished.usage, calibrated.usage);
    }
}
