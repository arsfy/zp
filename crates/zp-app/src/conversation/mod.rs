//! A conversation: one coding session shown as a transcript with a composer.
//!
//! The agent runs on Tokio; its events are forwarded through a channel and applied to the
//! [`Transcript`] in batches, at most ~30 times per second, so streaming does not redraw
//! for every token.

mod files;
mod permissions;
mod render;
pub mod transcript;
pub mod tree;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Subscription, Task, Window, px,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, TextChanged};
use zp_agent::AgentEvent;
use zp_ai::{Model, ThinkingLevel};
use zp_core::{CodingSession, CodingSessionOptions, UsageTotals, session_model};
use zp_session::Session;

use self::transcript::{Changes, Transcript};
use crate::actions::{ClosePicker, PickFirstModel, SendMessage, StopRun};
use crate::diff_view::DiffItem;
use crate::markdown::{Block, parse};
use crate::store::{AppModels, AppPaths};

/// Minimum time between applying event batches.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// A session handed over to the window (e.g. a fork to open in a new tab).
pub type SessionHandoff = Rc<RefCell<Option<Session>>>;

/// Events for the window around the conversation.
#[derive(Clone)]
pub enum ConversationEvent {
    /// Model, reasoning level, usage or running state changed.
    StatusChanged,
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
    /// The user asked for the full diff of a change.
    OpenDiff(DiffItem),
    /// The user picked a permission mode (for the whole project).
    PermissionModeChanged(zp_core::permissions::PermissionMode),
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
    pub thinking: ThinkingLevel,
    pub usage: UsageTotals,
    pub running: bool,
}

/// A model offered by the model picker.
#[derive(Clone, Debug)]
pub struct ModelChoice {
    pub model: Model,
    pub provider_name: String,
}

pub struct ConversationView {
    root: PathBuf,
    coding: Option<Arc<CodingSession>>,
    unavailable: Option<String>,
    transcript: Transcript,
    list_state: ListState,
    running: bool,
    /// Error from the last attempt to start a run.
    run_error: Option<String>,
    composer: Entity<EditableTextState>,
    model_filter: Entity<EditableTextState>,
    picker: Option<Picker>,
    /// When a popover was last closed by clicking outside it; a click on its own toggle
    /// button right after must not reopen it.
    picker_closed_at: Option<std::time::Instant>,
    /// Thinking blocks the user expanded, by (row, block).
    expanded_thinking: HashSet<(usize, usize)>,
    /// Tool cards the user toggled, by call id (overrides the default).
    tool_expanded: HashMap<String, bool>,
    markdown_cache: RefCell<HashMap<u64, Rc<Vec<Block>>>>,
    /// Diffs of tool calls, by call id.
    tool_diffs: RefCell<HashMap<String, Rc<Result<files::ToolDiff, String>>>>,
    /// Tool calls waiting for the user.
    approvals: Vec<permissions::Approval>,
    /// Whether the session tree overlay is shown.
    tree_open: bool,
    /// A model title was already requested for this session.
    title_requested: bool,
    focus_handle: FocusHandle,
    _agent_events: Option<zp_agent::Subscription>,
    _pump: Option<Task<()>>,
    _approvals: Option<Task<()>>,
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
        let model_filter = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        // The composer redraws itself while typing; only the model filter changes our list.
        let subscriptions =
            vec![cx.subscribe(&model_filter, |_, _, _: &TextChanged, cx| cx.notify())];
        let mut this = Self {
            root,
            coding: None,
            unavailable: None,
            transcript: Transcript::default(),
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            running: false,
            run_error: None,
            composer,
            model_filter,
            picker: None,
            picker_closed_at: None,
            expanded_thinking: HashSet::new(),
            tool_expanded: HashMap::new(),
            markdown_cache: RefCell::default(),
            tool_diffs: RefCell::default(),
            approvals: Vec::new(),
            tree_open: false,
            title_requested: false,
            focus_handle: cx.focus_handle(),
            _agent_events: None,
            _pump: None,
            _approvals: None,
            run_task: None,
            _subscriptions: subscriptions,
        };
        match this.start_session(session, cx) {
            Ok(()) => {}
            Err(error) => this.unavailable = Some(format!("{error:#}")),
        }
        this.focus_composer(window, cx);
        this
    }

    fn start_session(&mut self, session: Session, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let models = cx
            .try_global::<AppModels>()
            .map(|m| m.models.clone())
            .ok_or_else(|| anyhow::anyhow!("models are not initialized"))?;
        let model = match session_model(&models, &session) {
            Some(_) => None,
            None => Some(AppModels::default_model(cx).ok_or_else(|| {
                anyhow::anyhow!(
                    "No model is configured. Set an API key such as ANTHROPIC_API_KEY, \
                     OPENROUTER_API_KEY or DEEPSEEK_API_KEY and restart ZP."
                )
            })?),
        };
        let mut options = CodingSessionOptions::new(models, &self.root, session);
        if model.is_some() {
            // A new conversation: start at the default reasoning level.
            options.thinking_level = crate::store::AppSettings::get(cx).default_thinking;
        }
        options.model = model;
        options.global_context_dir = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone());
        options.blobs = AppPaths::blobs(cx);
        options.trusted = crate::approvals::is_trusted(&self.root, cx);
        let (approval_sender, mut approval_receiver) = futures::channel::mpsc::unbounded();
        options.permissions = Some(Arc::new(zp_core::permissions::PermissionGate::new(
            crate::approvals::project_policy(&self.root, cx),
            Arc::new(crate::approvals::ChannelApprover::new(approval_sender)),
            options.models.clone(),
        )));
        let coding = Arc::new(CodingSession::new(options)?);
        let approvals = cx.spawn(async move |this, cx| {
            while let Some(pending) = approval_receiver.next().await {
                if this
                    .update(cx, |this, cx| this.add_approval(pending, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self._approvals = Some(approvals);

        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<AgentEvent>();
        let subscription = coding.agent().subscribe(move |event| {
            let _ = sender.unbounded_send(event.clone());
        });
        let pump = cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.next().await {
                let mut batch = vec![first];
                while let Ok(event) = receiver.try_recv() {
                    batch.push(event);
                }
                if this
                    .update(cx, |this, cx| this.apply_events(batch, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(FRAME_INTERVAL).await;
            }
        });

        self.transcript = Transcript::from_messages(&coding.agent().messages());
        self.list_state = ListState::new(self.transcript.len(), ListAlignment::Top, px(1000.));
        self.list_state.set_follow_mode(gpui::FollowMode::Tail);
        self.coding = Some(coding);
        self._agent_events = Some(subscription);
        self._pump = Some(pump);
        Ok(())
    }

    fn apply_events(&mut self, events: Vec<AgentEvent>, cx: &mut Context<Self>) {
        let old_len = self.transcript.len();
        let mut changes = Changes::default();
        for event in &events {
            self.transcript.apply(event, &mut changes);
        }
        for &row in changes.dirty.iter().filter(|&&row| row < old_len) {
            self.list_state.splice(row..row + 1, 1);
        }
        let new_len = self.transcript.len();
        if new_len > old_len {
            self.list_state.splice(old_len..old_len, new_len - old_len);
        }
        if changes.usage {
            cx.emit(ConversationEvent::StatusChanged);
        }
        let files_changed = events.iter().any(|event| {
            matches!(event, AgentEvent::ToolExecutionEnd { result, .. } if result.details.is_some())
        });
        if files_changed {
            cx.emit(ConversationEvent::FilesChanged);
        }
        if changes.finished && self.running {
            self.finish_run(cx);
        }
        cx.notify();
    }

    /// Rebuilds the rows from the agent's transcript (after retry or branch changes).
    fn reload_transcript(&mut self) {
        if let Some(coding) = &self.coding {
            self.transcript = Transcript::from_messages(&coding.agent().messages());
            self.list_state.reset(self.transcript.len());
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
        self.transcript
            .rows()
            .iter()
            .find_map(|row| match row {
                transcript::Row::User { text, .. } => Some(zp_tools::shorten(text, 32)),
                _ => None,
            })
            .unwrap_or_else(|| "New Chat".to_owned())
    }

    /// Replaces the composer text (tests).
    #[cfg(test)]
    pub fn set_draft(&self, text: &str, cx: &mut App) {
        self.composer
            .update(cx, |composer, cx| composer.emplace(text, cx));
    }

    /// The composer text (tests).
    #[cfg(test)]
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

    /// Feeds scripted agent events, as if a run were in progress (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn inject(&mut self, events: Vec<AgentEvent>, running: bool, cx: &mut Context<Self>) {
        self.running = running;
        self.apply_events(events, cx);
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

    /// Stops the running agent, if any.
    pub fn abort(&mut self, cx: &mut Context<Self>) {
        if let Some(coding) = &self.coding {
            coding.agent().abort();
        }
        cx.notify();
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
    pub fn restore_leaf(&mut self, leaf: &str, cx: &mut Context<Self>) {
        if let Some(coding) = &self.coding
            && coding.session().entry(leaf).is_some()
            && coding.branch(leaf).is_ok()
        {
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

    /// Rewinds to before user message `row` and puts its text back in the composer.
    pub(crate) fn edit_and_resend(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(transcript::Row::User { text, .. }) = self.transcript.rows().get(row).cloned()
        else {
            return;
        };
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
                this.composer
                    .update(cx, |composer, cx| composer.emplace(&text, cx));
                this.focus_composer(window, cx);
                this.after_branch_change(result, cx);
            }),
            window,
            cx,
        );
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
        ConversationStatus {
            model: agent.map(|agent| agent.model()),
            thinking: agent.map_or(ThinkingLevel::Off, |agent| agent.thinking_level()),
            usage: self.transcript.usage().clone(),
            running: self.running,
        }
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

    /// Models the picker offers: those of providers with credentials.
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

    fn filtered_models(&self, cx: &App) -> Vec<ModelChoice> {
        let filter = self.model_filter.read(cx).as_str().trim().to_lowercase();
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

    fn parsed_markdown(&self, text: &str) -> Rc<Vec<Block>> {
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
        let text = self.composer.read(cx).as_str().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(coding) = self.coding.clone() else {
            return;
        };
        self.composer
            .update(cx, |composer, cx| composer.emplace("", cx));
        self.picker = None;
        cx.emit(ConversationEvent::Activity);
        if self.running {
            // Typed while working: delivered after the current tool calls finish.
            coding.agent().steer(text);
            cx.notify();
            return;
        }
        self.start_run(cx, move |coding| async move { coding.prompt(text).await });
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
        self.running = true;
        self.run_error = None;
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
        self.clear_approvals();
        let failed = self.has_failed();
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
        let model = zp_core::title::title_model(&models, &coding.agent().model());
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

    fn pick_first_model(
        &mut self,
        _: &PickFirstModel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(choice) = self.filtered_models(cx).into_iter().next() {
            self.select_model(choice.model, window, cx);
        }
    }

    pub(crate) fn retry(&mut self, cx: &mut Context<Self>) {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        if self.running {
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
            }
            // Keep the requested level valid for the new model.
            let clamped = model.clamp_thinking_level(level);
            if clamped != level {
                let _ = coding.set_thinking_level(clamped);
            }
        }
        self.picker = None;
        self.focus_composer(window, cx);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
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
        cx: &mut Context<Self>,
    ) {
        if !self.expanded_thinking.remove(&(row, block)) {
            self.expanded_thinking.insert((row, block));
        }
        self.list_state.splice(row..row + 1, 1);
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
        self.list_state.splice(row..row + 1, 1);
        cx.notify();
    }
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
