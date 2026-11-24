//! The Git view of the Changes panel: staged and unstaged files with checkboxes, the
//! commit message and commit, the branch picker, push / pull / fetch, undoing the last
//! commit, and the history with each commit's diff.

mod watch;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ColorExt as _, Context, DismissEvent, Entity,
    EventEmitter, Focusable as _, FontWeight, IntoElement, Pixels, Point, PromptLevel, Render,
    Rgba, SharedString, Subscription, Task, UniformListScrollHandle, Window, anchored, deferred,
    div, prelude::*, px, rgb_to_hsla, svg, uniform_list,
};
use gpui_elements::editable_text::{
    EditableTextState, StringStorage, TextChanged, text_area, text_input,
};
use zp_core::changes::RevertStatus;
use zp_core::git::{
    Branch, Commit, CommitOptions, FileText, GitFile, GitFileStatus, GitStatus, file_in_worktree,
};

use crate::context_menu::ContextMenu;
use crate::diff_view::DiffItem;
use crate::store::AppModels;
use crate::theme::ActiveTheme;
use crate::ui::tooltip;

const ICON_BRANCH: &[u8] = include_bytes!("../../../assets/icons/git-branch.svg");
const ICON_UNDO: &[u8] = include_bytes!("../../../assets/icons/undo-2.svg");
const ICON_DISCARD: &[u8] = include_bytes!("../../../assets/icons/trash-2.svg");
const ICON_CHEVRON: &[u8] = include_bytes!("../../../assets/icons/chevron-down.svg");
const ICON_CHEVRON_RIGHT: &[u8] = include_bytes!("../../../assets/icons/chevron-right.svg");
const ICON_CHEVRON_UP: &[u8] = include_bytes!("../../../assets/icons/chevron-up.svg");
const ICON_MAXIMIZE: &[u8] = include_bytes!("../../../assets/icons/maximize-2.svg");
const ICON_MINIMIZE: &[u8] = include_bytes!("../../../assets/icons/minimize-2.svg");
const ICON_COMMIT: &[u8] = include_bytes!("../../../assets/icons/git-commit-horizontal.svg");
const ICON_ARROW_UP: &[u8] = include_bytes!("../../../assets/icons/arrow-up.svg");
const ICON_CHECK: &[u8] = include_bytes!("../../../assets/icons/check.svg");
const ICON_STAR: &[u8] = include_bytes!("../../../assets/icons/star.svg");
/// Height of file and section rows.
const ROW_HEIGHT: f32 = 26.;
const ICON_ARROW_DOWN: &[u8] = include_bytes!("../../../assets/icons/arrow-down.svg");
/// The remote button: an arrow icon with a commit count, and the action's name.
type RemoteButton = (Option<(&'static [u8], usize)>, &'static str);

/// A git operation on paths of the repository.
type PathsOp = fn(&Path, &[String]) -> anyhow::Result<()>;

/// Commits listed in the history.
const HISTORY_LIMIT: usize = 200;

/// The two views of the Git panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitTab {
    Changes,
    History,
}

/// Which side of a file a row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Staged,
    Unstaged,
}

#[derive(PartialEq, Eq)]
enum Load {
    Loading,
    NotRepository,
    Loaded(GitStatus),
    Failed(String),
}

#[derive(Clone, Copy)]
enum Operation {
    Staging,
    Other(&'static str),
}

#[derive(Clone, Copy)]
struct ControlState {
    disabled: bool,
    dimmed: bool,
}

pub enum GitPanelEvent {
    /// Show these diffs, starting at `index`.
    OpenDiff { items: Vec<DiffItem>, index: usize },
    /// The branch or commits changed (the window title shows the branch).
    RepositoryChanged,
}

struct OpenMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _dismiss: Subscription,
}

struct BranchPicker {
    filter: Entity<EditableTextState>,
    /// `None` while loading.
    branches: Option<Vec<Branch>>,
    _changes: Subscription,
}

/// A diff to open, read off the main thread.
struct DiffData {
    path: PathBuf,
    display: String,
    caption: String,
    before: FileText,
    after: FileText,
}

impl DiffData {
    /// The diff view's item; a version too large to compare shows the whole file as too
    /// large (not as added or deleted).
    fn into_item(self) -> DiffItem {
        let too_large = self.before == FileText::TooLarge || self.after == FileText::TooLarge;
        let text = |side: FileText| side.into_text().filter(|_| !too_large).map(Arc::from);
        DiffItem {
            path: self.path,
            display: self.display,
            caption: self.caption,
            before: text(self.before),
            after: text(self.after),
            too_large,
            status: RevertStatus::Active,
            revert: None,
        }
    }
}

pub struct GitPanel {
    root: PathBuf,
    tab: GitTab,
    status: Load,
    /// `None` until the History tab is shown.
    history: Option<Vec<Commit>>,
    message: Entity<EditableTextState>,
    /// Staging blocks overlapping operations without changing the controls' appearance.
    busy: Option<Operation>,
    /// Feedback of the last operation: text and whether it is an error.
    notice: Option<(String, bool)>,
    staged_open: bool,
    unstaged_open: bool,
    /// Amend, sign off, skip hooks.
    options: CommitOptions,
    /// The last commit's message, filled in when Amend was turned on.
    amend_prefill: Option<String>,
    /// Only the commit controls are shown, without the message.
    commit_collapsed: bool,
    /// The message takes the whole panel.
    commit_maximized: bool,
    menu: Option<OpenMenu>,
    branch_picker: Option<BranchPicker>,
    watch_task: Option<Task<()>>,
    status_task: Option<Task<()>>,
    status_refresh_pending: bool,
    history_task: Option<Task<()>>,
    history_refresh_pending: bool,
    refresh_seq: u64,
    history_seq: u64,
    /// Scroll position of the history list.
    history_scroll: UniformListScrollHandle,
    op_task: Option<Task<()>>,
    /// The commit message model is writing a message.
    generating: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

impl GitPanel {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let subscriptions = vec![cx.subscribe(&message, |_, _, _: &TextChanged, cx| cx.notify())];
        Self {
            root,
            tab: GitTab::Changes,
            status: Load::Loading,
            history: None,
            message,
            busy: None,
            notice: None,
            staged_open: true,
            unstaged_open: true,
            options: CommitOptions::default(),
            amend_prefill: None,
            commit_collapsed: false,
            commit_maximized: false,
            menu: None,
            branch_picker: None,
            watch_task: None,
            status_task: None,
            status_refresh_pending: false,
            history_task: None,
            history_refresh_pending: false,
            refresh_seq: 0,
            history_seq: 0,
            history_scroll: UniformListScrollHandle::new(),
            op_task: None,
            generating: None,
            _subscriptions: subscriptions,
        }
    }

    // ----- state -----------------------------------------------------------------------

    /// The loaded status, if any.
    pub fn status(&self) -> Option<&GitStatus> {
        match &self.status {
            Load::Loaded(status) => Some(status),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn tab(&self) -> GitTab {
        self.tab
    }

    /// The loaded history (tests).
    #[cfg(test)]
    pub fn history(&self) -> Option<&[Commit]> {
        self.history.as_deref()
    }

    /// Whether an operation is running (tests).
    #[cfg(test)]
    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// The last operation's feedback (tests).
    #[cfg(test)]
    pub fn notice(&self) -> Option<&(String, bool)> {
        self.notice.as_ref()
    }

    /// The commit message input (tests).
    #[cfg(test)]
    pub fn message_input(&self) -> Entity<EditableTextState> {
        self.message.clone()
    }

    /// The branch names in the picker, once loaded (tests).
    #[cfg(test)]
    pub fn picker_branches(&self) -> Option<Vec<String>> {
        let picker = self.branch_picker.as_ref()?;
        Some(
            picker
                .branches
                .as_ref()?
                .iter()
                .map(|b| b.name.clone())
                .collect(),
        )
    }

    /// The open dropdown menu (tests).
    #[cfg(test)]
    pub fn menu(&self) -> Option<Entity<ContextMenu>> {
        self.menu.as_ref().map(|open| open.menu.clone())
    }

    /// The commit options (tests).
    #[cfg(test)]
    pub fn commit_options(&self) -> CommitOptions {
        self.options
    }

    fn repo_root(&self) -> Option<PathBuf> {
        self.status().map(|status| status.root.clone())
    }

    fn busy_label(&self) -> Option<&'static str> {
        match self.busy {
            Some(Operation::Other(label)) => Some(label),
            _ => None,
        }
    }

    fn control_state(&self, unavailable: bool) -> ControlState {
        ControlState {
            disabled: unavailable || self.busy.is_some(),
            dimmed: unavailable || self.busy_label().is_some(),
        }
    }

    /// What a commit takes: the staged files, or else the changed tracked files.
    fn commit_files(&self) -> Vec<&GitFile> {
        let Some(status) = self.status() else {
            return Vec::new();
        };
        let staged: Vec<&GitFile> = status.staged().collect();
        if !staged.is_empty() {
            return staged;
        }
        status
            .unstaged()
            .filter(|file| !file.is_untracked())
            .collect()
    }

    /// The message used when the input is empty: `Update main.rs`, `Update 3 files`.
    pub fn suggested_message(&self) -> String {
        match self.commit_files()[..] {
            [] => String::new(),
            [file] => {
                let verb = match file.status() {
                    GitFileStatus::Added | GitFileStatus::Untracked => "Create",
                    GitFileStatus::Deleted => "Delete",
                    GitFileStatus::Renamed => "Rename",
                    _ => "Update",
                };
                format!("{verb} {}", file_name(&file.repo_path))
            }
            ref files => format!("Update {} files", files.len()),
        }
    }

    /// The commit button's label: Amend, Commit, or Commit Tracked (nothing staged).
    fn commit_label(&self) -> &'static str {
        if self.options.amend {
            "Amend"
        } else if self.status().is_some_and(|s| s.staged().next().is_none()) {
            "Commit Tracked"
        } else {
            "Commit"
        }
    }

    // ----- loading ---------------------------------------------------------------------

    /// Watches the worktree and Git metadata while the Git view is visible.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if visible == self.watch_task.is_some() {
            return;
        }
        self.watch_task = visible.then(|| {
            let (changes, mut changed) = tokio::sync::mpsc::channel(1);
            let executor = cx.background_executor().clone();
            let monitor =
                executor.spawn(watch::monitor(self.root.clone(), executor.clone(), changes));
            cx.spawn(async move |this, cx| {
                let _monitor = monitor;
                while let Some(change) = changed.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            if change == watch::Change::Status {
                                this.refresh(cx);
                            } else {
                                this.refresh_repository(cx);
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
    }

    /// Reloads status in the background, keeping one follow-up for changes during a load
    /// or an active operation.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.status_task.is_some() {
            self.status_refresh_pending = true;
            return;
        }
        self.refresh_seq += 1;
        let status_seq = self.refresh_seq;
        let root = self.root.clone();
        let status = cx
            .background_executor()
            .spawn(async move { zp_core::git::status(&root) });
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let result = status.await;
            let _ = this.update(cx, |this, cx| {
                this.status_task = None;
                if this.busy.is_some() {
                    this.status_refresh_pending = true;
                    return;
                }
                let refresh_again = std::mem::take(&mut this.status_refresh_pending);
                let status = match result {
                    Ok(Some(status)) => Load::Loaded(status),
                    Ok(None) => Load::NotRepository,
                    Err(error) => Load::Failed(format!("{error:#}")),
                };
                if status != this.status {
                    let revision_changed = match (&this.status, &status) {
                        (Load::Loaded(old), Load::Loaded(new)) => {
                            old.root != new.root || old.head != new.head || old.branch != new.branch
                        }
                        (Load::Loaded(_) | Load::Failed(_), Load::NotRepository)
                        | (_, Load::Loaded(_)) => true,
                        _ => false,
                    };
                    this.status = status;
                    if revision_changed {
                        if this.history_seq < status_seq {
                            this.refresh_loaded_history(cx);
                        }
                        cx.emit(GitPanelEvent::RepositoryChanged);
                    }
                    cx.notify();
                }
                if refresh_again {
                    this.refresh(cx);
                }
            });
        }));
    }

    /// Also reloads history when the History tab is shown (or invalidates cached history).
    pub fn refresh_repository(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        self.refresh_loaded_history(cx);
    }

    fn refresh_loaded_history(&mut self, cx: &mut Context<Self>) {
        if self.tab != GitTab::History {
            self.history = None;
            self.history_task = None;
            self.history_refresh_pending = false;
            return;
        }
        self.load_history(cx);
    }

    fn load_history(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.history_task.is_some() {
            self.history_refresh_pending = true;
            return;
        }
        self.refresh_seq += 1;
        self.history_seq = self.refresh_seq;
        let root = self.root.clone();
        let task = cx.background_executor().spawn(async move {
            let root = zp_core::git::repo_root(&root).unwrap_or(root);
            zp_core::git::history(&root, HISTORY_LIMIT)
        });
        self.history_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.history_task = None;
                if this.tab != GitTab::History {
                    this.history = None;
                    this.history_refresh_pending = false;
                    return;
                }
                if this.busy.is_some() {
                    this.history_refresh_pending = true;
                    return;
                }
                let refresh_again = std::mem::take(&mut this.history_refresh_pending);
                match result {
                    Ok(commits) => this.history = Some(commits),
                    Err(error) => {
                        this.history = Some(Vec::new());
                        this.notice = Some((format!("{error:#}"), true));
                    }
                }
                cx.notify();
                if refresh_again {
                    this.load_history(cx);
                }
            });
        }));
    }

    pub fn set_tab(&mut self, tab: GitTab, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == GitTab::History {
            self.load_history(cx);
        } else {
            self.history = None;
            self.history_task = None;
            self.history_refresh_pending = false;
        }
        cx.notify();
    }

    fn finish_busy(&mut self, cx: &mut Context<Self>) {
        self.busy = None;
        if std::mem::take(&mut self.status_refresh_pending) {
            self.refresh(cx);
        }
        if std::mem::take(&mut self.history_refresh_pending) {
            self.refresh_loaded_history(cx);
        }
    }

    // ----- operations ------------------------------------------------------------------

    /// Runs `op` (given the repository root) in the background, then reloads.
    fn run(
        &mut self,
        operation: Operation,
        op: impl FnOnce(&Path) -> anyhow::Result<()> + Send + 'static,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(root) = self.repo_root() else {
            return;
        };
        self.busy = Some(operation);
        self.notice = None;
        self.menu = None;
        let task = cx.background_executor().spawn(async move { op(&root) });
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                let reload_history = std::mem::take(&mut this.history_refresh_pending)
                    || !matches!(operation, Operation::Staging);
                this.status_refresh_pending = false;
                match result {
                    Ok(()) => on_success(this, cx),
                    Err(error) => this.notice = Some((format!("{error:#}"), true)),
                }
                if reload_history {
                    this.refresh_repository(cx);
                } else {
                    this.refresh(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Stages or unstages `paths` (relative to the repository root).
    pub fn set_staged(&mut self, paths: Vec<String>, staged: bool, cx: &mut Context<Self>) {
        let op: PathsOp = if staged {
            zp_core::git::stage
        } else {
            zp_core::git::unstage
        };
        self.run(
            Operation::Staging,
            move |root| op(root, &paths),
            |_, _| {},
            cx,
        );
    }

    /// Stages or unstages everything under the project.
    pub fn set_all_staged(&mut self, staged: bool, cx: &mut Context<Self>) {
        let dir = self.root.clone();
        if staged {
            self.run(
                Operation::Staging,
                move |_| zp_core::git::stage_all(&dir),
                |_, _| {},
                cx,
            );
        } else {
            self.run(
                Operation::Staging,
                move |_| zp_core::git::unstage_all(&dir),
                |_, _| {},
                cx,
            );
        }
    }

    /// Commits the staged changes (the changed tracked files when nothing is staged) with
    /// the typed message or the suggested one, using the commit options.
    pub fn commit(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.status() else {
            return;
        };
        let options = self.options;
        let typed = self.message.read(cx).as_str().trim().to_owned();
        let message = if typed.is_empty() && !options.amend {
            self.suggested_message()
        } else {
            typed
        };
        let stage_tracked = status.staged().next().is_none() && !options.amend;
        if stage_tracked && self.commit_files().is_empty() {
            self.notice = Some(("Nothing to commit".into(), true));
            cx.notify();
            return;
        }
        let dir = self.root.clone();
        self.run(
            Operation::Other(if options.amend {
                "Amending…"
            } else {
                "Committing…"
            }),
            move |root| {
                if stage_tracked {
                    zp_core::git::stage_tracked(&dir)?;
                }
                zp_core::git::commit(root, &message, options)
            },
            |this, cx| {
                this.message.update(cx, |input, cx| input.emplace("", cx));
                this.options.amend = false;
                this.amend_prefill = None;
            },
            cx,
        );
    }

    /// Stops waiting for the commit message model.
    pub fn cancel_commit_message(&mut self, cx: &mut Context<Self>) {
        // Dropping the task aborts the request.
        self.generating = None;
        self.notice = None;
        self.finish_busy(cx);
        cx.notify();
    }

    /// Asks the commit message model for a message for what a commit would take, in a
    /// session that can only read the project, and fills the input with it; the commit
    /// is still made with the button.
    pub fn generate_commit_message(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let model = match AppModels::commit_model(cx) {
            Ok(model) => model,
            Err(error) => {
                self.notice = Some((error.to_string(), true));
                cx.notify();
                return;
            }
        };
        // The reply replaces the draft only while the draft is still what it was.
        let draft = self.message.read(cx).as_str().to_owned();
        let models = cx.global::<AppModels>().models.clone();
        let dir = self.root.clone();
        let amend = self.options.amend;
        self.busy = Some(Operation::Other("Writing message…"));
        self.notice = None;
        let task = gpui_tokio::Tokio::spawn(cx, async move {
            zp_core::commit_message::generate_commit_message(models, &dir, &model, amend).await
        });
        self.generating = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.generating = None;
                this.finish_busy(cx);
                match result {
                    Ok(Ok(message)) => {
                        if this.message.read(cx).as_str() == draft {
                            this.message
                                .update(cx, |input, cx| input.emplace(&message, cx));
                            // The message is the user's now: turning Amend off keeps it.
                            this.amend_prefill = None;
                            this.commit_collapsed = false;
                        } else {
                            this.notice = Some((
                                "The message was edited meanwhile; the generated one was \
                                 dropped."
                                    .into(),
                                false,
                            ));
                        }
                    }
                    Ok(Err(error)) => this.notice = Some((format!("{error:#}"), true)),
                    Err(error) => {
                        this.notice =
                            Some((format!("the request stopped unexpectedly: {error}"), true))
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Turns Amend on (filling in the last commit's message when the input is empty) or
    /// off (removing that message again if unchanged).
    pub fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.options.amend = !self.options.amend;
        if !self.options.amend {
            let unchanged = self
                .amend_prefill
                .take()
                .is_some_and(|prefill| self.message.read(cx).as_str() == prefill);
            if unchanged {
                self.message.update(cx, |input, cx| input.emplace("", cx));
            }
            cx.notify();
            return;
        }
        if !self.message.read(cx).as_str().trim().is_empty() {
            cx.notify();
            return;
        }
        let Some(root) = self.repo_root() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { zp_core::git::last_commit_message(&root) });
        cx.spawn(async move |this, cx| {
            let Ok(Some(message)) = task.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.options.amend && this.message.read(cx).as_str().trim().is_empty() {
                    this.message
                        .update(cx, |input, cx| input.emplace(&message, cx));
                    this.amend_prefill = Some(message);
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub fn toggle_signoff(&mut self, cx: &mut Context<Self>) {
        self.options.signoff = !self.options.signoff;
        cx.notify();
    }

    pub fn toggle_skip_hooks(&mut self, cx: &mut Context<Self>) {
        self.options.no_verify = !self.options.no_verify;
        cx.notify();
    }

    /// Undoes the last commit, keeping its changes staged; asks first when it was
    /// already pushed. Its message comes back into the input.
    pub fn uncommit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = self.status() else {
            return;
        };
        let Some(summary) = status.last_commit.clone() else {
            return;
        };
        let pushed = status.upstream.clone().filter(|_| status.ahead == 0);
        let Some(upstream) = pushed else {
            self.run_uncommit(summary, cx);
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            "Undo a pushed commit?",
            Some(&format!(
                "“{summary}” is already on {upstream}. Undoing it here makes the branch differ from the remote."
            )),
            &["Uncommit", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.run_uncommit(summary, cx));
            }
        })
        .detach();
    }

    fn run_uncommit(&mut self, summary: String, cx: &mut Context<Self>) {
        self.run(
            Operation::Other("Uncommitting…"),
            zp_core::git::uncommit,
            move |this, cx| {
                if this.message.read(cx).as_str().trim().is_empty() {
                    this.message
                        .update(cx, |input, cx| input.emplace(&summary, cx));
                }
            },
            cx,
        );
    }

    /// Throws away the unstaged changes of `files` after asking: tracked files go back to
    /// the index, untracked ones to the trash.
    pub fn discard(&mut self, files: Vec<GitFile>, window: &mut Window, cx: &mut Context<Self>) {
        if files.is_empty() {
            return;
        }
        let title = match &files[..] {
            [file] => format!("Discard changes to {}?", file_name(&file.repo_path)),
            _ => format!("Discard changes to {} files?", files.len()),
        };
        let untracked = files.iter().filter(|f| f.is_untracked()).count();
        let detail = if untracked == files.len() {
            "New files are moved to the trash.".to_owned()
        } else if untracked > 0 {
            "Unstaged changes are lost (staged ones are kept); new files are moved to the trash."
                .to_owned()
        } else {
            "Unstaged changes are lost; staged changes are kept.".to_owned()
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &["Discard", "Cancel"],
            cx,
        );
        let trash = cx
            .try_global::<crate::store::FileTrash>()
            .copied()
            .unwrap_or_else(crate::store::FileTrash::system);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.run(
                    Operation::Other("Discarding…"),
                    move |root| {
                        let (new, tracked): (Vec<GitFile>, Vec<GitFile>) =
                            files.into_iter().partition(GitFile::is_untracked);
                        for file in &new {
                            (trash.0)(&file.path)?;
                        }
                        let paths: Vec<String> = tracked.into_iter().map(|f| f.repo_path).collect();
                        zp_core::git::discard(root, &paths)
                    },
                    |_, _| {},
                    cx,
                )
            });
        })
        .detach();
    }

    pub fn push(&mut self, cx: &mut Context<Self>) {
        self.run(
            Operation::Other("Pushing…"),
            zp_core::git::push,
            |_, _| {},
            cx,
        );
    }

    pub fn pull(&mut self, cx: &mut Context<Self>) {
        self.run(
            Operation::Other("Pulling…"),
            zp_core::git::pull,
            |_, _| {},
            cx,
        );
    }

    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        self.run(
            Operation::Other("Fetching…"),
            zp_core::git::fetch,
            |_, _| {},
            cx,
        );
    }

    // ----- branches --------------------------------------------------------------------

    pub fn open_branch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.repo_root() else {
            return;
        };
        let filter = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        filter.read(cx).focus_handle(cx).focus(window, cx);
        let changes = cx.subscribe(&filter, |_, _, _: &TextChanged, cx| cx.notify());
        self.branch_picker = Some(BranchPicker {
            filter,
            branches: None,
            _changes: changes,
        });
        let task = cx
            .background_executor()
            .spawn(async move { zp_core::git::branches(&root) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(branches) => {
                        if let Some(picker) = &mut this.branch_picker {
                            picker.branches = Some(branches);
                        }
                    }
                    Err(error) => {
                        this.branch_picker = None;
                        this.notice = Some((format!("{error:#}"), true));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn close_branch_picker(&mut self, cx: &mut Context<Self>) {
        self.branch_picker = None;
        cx.notify();
    }

    /// Branches matching the picker's filter, and whether the filter names a new branch.
    fn picker_choices(&self, cx: &App) -> (Vec<Branch>, Option<String>) {
        let Some(picker) = &self.branch_picker else {
            return (Vec::new(), None);
        };
        let filter = picker.filter.read(cx).as_str().trim().to_owned();
        let needle = filter.to_lowercase();
        let branches = picker.branches.as_deref().unwrap_or_default();
        let matches: Vec<Branch> = branches
            .iter()
            .filter(|b| b.name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        let exists = branches.iter().any(|b| !b.remote && b.name == filter);
        let create = (!filter.is_empty() && !exists && !filter.contains(char::is_whitespace))
            .then_some(filter);
        (matches, create)
    }

    /// Enter in the filter: switches to the first match, or creates the typed branch.
    pub fn confirm_branch(&mut self, cx: &mut Context<Self>) {
        let (matches, create) = self.picker_choices(cx);
        if let Some(branch) = matches.into_iter().next() {
            self.switch_branch(branch, cx);
        } else if let Some(name) = create {
            self.create_branch(name, cx);
        }
    }

    pub fn switch_branch(&mut self, branch: Branch, cx: &mut Context<Self>) {
        self.branch_picker = None;
        if branch.current {
            cx.notify();
            return;
        }
        self.run(
            Operation::Other("Switching…"),
            move |root| zp_core::git::switch_branch(root, &branch),
            |_, _| {},
            cx,
        );
    }

    pub fn create_branch(&mut self, name: String, cx: &mut Context<Self>) {
        self.branch_picker = None;
        self.run(
            Operation::Other("Creating branch…"),
            move |root| zp_core::git::create_branch(root, &name),
            |_, _| {},
            cx,
        );
    }

    // ----- diffs -----------------------------------------------------------------------

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Reads diffs in the background and opens them at `index`.
    fn open_diffs(
        &mut self,
        read: impl FnOnce() -> anyhow::Result<Vec<DiffData>> + Send + 'static,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let task = cx.background_executor().spawn(async move { read() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(data) if !data.is_empty() => {
                    let items = data
                        .into_iter()
                        .map(DiffData::into_item)
                        .collect::<Vec<_>>();
                    let index = index.min(items.len() - 1);
                    cx.emit(GitPanelEvent::OpenDiff { items, index });
                }
                Ok(_) => {}
                Err(error) => {
                    this.notice = Some((format!("{error:#}"), true));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The diffs of one section (staged: index vs `HEAD`; unstaged: working tree vs
    /// index), opened at `file`.
    pub fn open_file(&mut self, file: &GitFile, side: Side, cx: &mut Context<Self>) {
        let Some(status) = self.status() else {
            return;
        };
        let files: Vec<GitFile> = match side {
            Side::Staged => status.staged().cloned().collect(),
            Side::Unstaged => status.unstaged().cloned().collect(),
        };
        let index = files
            .iter()
            .position(|f| f.repo_path == file.repo_path)
            .unwrap_or(0);
        let root = status.root.clone();
        let displays: Vec<String> = files.iter().map(|f| self.display(&f.path)).collect();
        self.open_diffs(
            move || {
                files
                    .into_iter()
                    .zip(displays)
                    .map(|(file, display)| {
                        let (caption, before, after) = match side {
                            Side::Staged => (
                                "Staged (index vs HEAD)",
                                zp_core::git::file_at(
                                    &root,
                                    "HEAD",
                                    file.renamed_from.as_deref().unwrap_or(&file.repo_path),
                                )?,
                                zp_core::git::file_at(&root, "", &file.repo_path)?,
                            ),
                            Side::Unstaged => (
                                "Unstaged (working tree vs index)",
                                if file.is_untracked() {
                                    FileText::Missing
                                } else {
                                    zp_core::git::file_at(&root, "", &file.repo_path)?
                                },
                                file_in_worktree(&file.path),
                            ),
                        };
                        Ok(DiffData {
                            path: file.path,
                            display,
                            caption: caption.to_owned(),
                            before,
                            after,
                        })
                    })
                    .collect()
            },
            index,
            cx,
        );
    }

    /// Every changed file, working tree vs `HEAD`.
    pub fn open_all(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.status() else {
            return;
        };
        let root = status.root.clone();
        let files = status.files.clone();
        let displays: Vec<String> = files.iter().map(|f| self.display(&f.path)).collect();
        self.open_diffs(
            move || {
                files
                    .into_iter()
                    .zip(displays)
                    .map(|(file, display)| {
                        Ok(DiffData {
                            before: zp_core::git::file_at(
                                &root,
                                "HEAD",
                                file.renamed_from.as_deref().unwrap_or(&file.repo_path),
                            )?,
                            after: file_in_worktree(&file.path),
                            path: file.path,
                            display,
                            caption: "Working tree vs HEAD".to_owned(),
                        })
                    })
                    .collect()
            },
            0,
            cx,
        );
    }

    /// The files a commit changed, against its first parent.
    pub fn open_commit(&mut self, commit: &Commit, cx: &mut Context<Self>) {
        let root = zp_core::git::repo_root(&self.root).unwrap_or_else(|| self.root.clone());
        let sha = commit.sha.clone();
        let caption = format!("{} {}", commit.short_sha, commit.summary);
        let project = self.root.clone();
        self.open_diffs(
            move || {
                let parent = format!("{sha}^");
                zp_core::git::commit_files(&root, &sha)?
                    .into_iter()
                    .map(|file| {
                        let path = root.join(&file.repo_path);
                        let display = path
                            .strip_prefix(&project)
                            .unwrap_or(Path::new(&file.repo_path))
                            .to_string_lossy()
                            .replace('\\', "/");
                        Ok(DiffData {
                            before: zp_core::git::file_at(
                                &root,
                                &parent,
                                file.renamed_from.as_deref().unwrap_or(&file.repo_path),
                            )?,
                            after: zp_core::git::file_at(&root, &sha, &file.repo_path)?,
                            path,
                            display,
                            caption: caption.clone(),
                        })
                    })
                    .collect()
            },
            0,
            cx,
        );
    }

    // ----- menus -----------------------------------------------------------------------

    fn show_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(ContextMenu, gpui::WeakEntity<Self>) -> ContextMenu,
    ) {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::new(window, cx, |menu| build(menu, this));
        let dismiss = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.menu = None;
            cx.notify();
        });
        self.menu = Some(OpenMenu {
            menu,
            position,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    fn stage_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let unstaged: Vec<GitFile> = self
            .status()
            .map(|s| s.unstaged().cloned().collect())
            .unwrap_or_default();
        self.show_menu(position, window, cx, move |menu, this| {
            let (stage, unstage, discard) = (this.clone(), this.clone(), this);
            let menu = menu
                .item("Stage All", move |_, cx| {
                    let _ = stage.update(cx, |this, cx| this.set_all_staged(true, cx));
                })
                .item("Unstage All", move |_, cx| {
                    let _ = unstage.update(cx, |this, cx| this.set_all_staged(false, cx));
                });
            if unstaged.is_empty() {
                return menu;
            }
            menu.separator()
                .destructive_item("Discard All Unstaged…", move |window, cx| {
                    let files = unstaged.clone();
                    let _ = discard.update(cx, |this, cx| this.discard(files, window, cx));
                })
        });
    }

    fn remote_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let publish = self.status().is_some_and(|s| s.upstream.is_none());
        self.show_menu(position, window, cx, move |menu, this| {
            let (fetch, pull, push) = (this.clone(), this.clone(), this);
            let menu = menu.item("Fetch", move |_, cx| {
                let _ = fetch.update(cx, |this, cx| this.fetch(cx));
            });
            let menu = if publish {
                menu
            } else {
                menu.item("Pull", move |_, cx| {
                    let _ = pull.update(cx, |this, cx| this.pull(cx));
                })
            };
            menu.item(
                if publish { "Publish Branch" } else { "Push" },
                move |_, cx| {
                    let _ = push.update(cx, |this, cx| this.push(cx));
                },
            )
        });
    }

    pub(crate) fn commit_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let can_amend = self.status().is_some_and(|s| s.head.is_some());
        let options = self.options;
        self.show_menu(position, window, cx, move |menu, this| {
            let (amend, signoff, hooks) = (this.clone(), this.clone(), this);
            let menu = if can_amend {
                menu.toggle_item("Amend", options.amend, move |_, cx| {
                    let _ = amend.update(cx, |this, cx| this.toggle_amend(cx));
                })
            } else {
                menu
            };
            menu.toggle_item("Signoff", options.signoff, move |_, cx| {
                let _ = signoff.update(cx, |this, cx| this.toggle_signoff(cx));
            })
            .toggle_item("Skip Hooks", options.no_verify, move |_, cx| {
                let _ = hooks.update(cx, |this, cx| this.toggle_skip_hooks(cx));
            })
        });
    }

    /// Opens the diff of the last commit.
    pub fn open_last_commit(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.repo_root() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { zp_core::git::history(&root, 1) });
        cx.spawn(async move |this, cx| {
            if let Ok(Some(commit)) = task.await.map(|mut h| h.pop()) {
                let _ = this.update(cx, |this, cx| this.open_commit(&commit, cx));
            }
        })
        .detach();
    }

    // ----- rendering -------------------------------------------------------------------

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let count = self.status().map_or(0, |s| s.files.len());
        let tab = |tab: GitTab, label: String, id: &'static str| {
            let active = self.tab == tab;
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .flex_1()
                .flex()
                .justify_center()
                .py_1()
                .cursor_pointer()
                .text_size(px(12.))
                .border_b_1()
                .border_color(if active { colors.accent } else { colors.border })
                .text_color(if active {
                    colors.text
                } else {
                    colors.text_muted
                })
                .hover_style(|s| s.text_color(colors.text))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
        };
        div()
            .flex()
            .child(tab(
                GitTab::Changes,
                if count > 0 {
                    format!("Changes ({count})")
                } else {
                    "Changes".to_owned()
                },
                "git-tab-changes",
            ))
            .child(tab(
                GitTab::History,
                "History".to_owned(),
                "git-tab-history",
            ))
    }

    fn render_changes(&self, status: &GitStatus, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (added, removed) = status.files.iter().fold((0, 0), |(a, r), f| {
            (
                a + f.staged_stats.0 + f.unstaged_stats.0,
                r + f.staged_stats.1 + f.unstaged_stats.1,
            )
        });
        let has_unstaged = status.unstaged().next().is_some();
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .text_size(px(12.))
            .child(
                div()
                    .id("git-view-diff")
                    .debug_selector(|| "git-view-diff".into())
                    .flex()
                    .gap_1p5()
                    .cursor_pointer()
                    .text_color(colors.text_muted)
                    .hover_style(|s| s.text_color(colors.text))
                    .child("± View Diff")
                    .child(stats(added, removed, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.open_all(cx))),
            )
            .child(div().flex_1())
            .child(split_button(
                "git-stage-all",
                div().child(if has_unstaged {
                    "Stage All"
                } else {
                    "Unstage All"
                }),
                self.control_state(false),
                cx,
                cx.listener(move |this, _, _, cx| this.set_all_staged(has_unstaged, cx)),
                cx.listener(|this, event: &ClickEvent, window, cx| {
                    this.stage_menu(event.position(), window, cx)
                }),
            ));

        let mut list = div().flex().flex_col();
        let staged: Vec<&GitFile> = status.staged().collect();
        let unstaged: Vec<&GitFile> = status.unstaged().collect();
        if !staged.is_empty() {
            list = list.child(self.render_section(Side::Staged, staged.len(), cx));
            if self.staged_open {
                for (index, file) in staged.iter().enumerate() {
                    list = list.child(self.render_file(file, Side::Staged, index, cx));
                }
            }
        }
        if !unstaged.is_empty() {
            list = list.child(self.render_section(Side::Unstaged, unstaged.len(), cx));
            if self.unstaged_open {
                for (index, file) in unstaged.iter().enumerate() {
                    list = list.child(self.render_file(file, Side::Unstaged, index, cx));
                }
            }
        }
        if status.files.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child("No changes"),
            );
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("git-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    fn render_section(&self, side: Side, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let open = match side {
            Side::Staged => self.staged_open,
            Side::Unstaged => self.unstaged_open,
        };
        let (label, id) = match side {
            Side::Staged => ("Staged", "git-section-staged"),
            Side::Unstaged => ("Unstaged", "git-section-unstaged"),
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_1p5()
            .px_3()
            .h(px(ROW_HEIGHT))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(colors.text_muted)
            .hover_style(|s| s.bg(colors.surface_hover))
            .child(
                svg()
                    .data(if open {
                        ICON_CHEVRON
                    } else {
                        ICON_CHEVRON_RIGHT
                    })
                    .size(px(12.))
                    .flex_shrink_0()
                    .text_color(colors.text_muted),
            )
            .child(div().flex_1().child(label))
            .child(
                div()
                    .h(px(16.))
                    .min_w(px(16.))
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(colors.border)
                    .text_size(px(10.))
                    .child(count.to_string()),
            )
            .child(
                checkbox(
                    format!("{id}-check"),
                    side == Side::Staged,
                    self.control_state(false),
                    cx,
                )
                .debug_selector(move || format!("{id}-check"))
                .tooltip(tooltip(match side {
                    Side::Staged => "Unstage all",
                    Side::Unstaged => "Stage all",
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_all_staged(side == Side::Unstaged, cx);
                })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                match side {
                    Side::Staged => this.staged_open = !this.staged_open,
                    Side::Unstaged => this.unstaged_open = !this.unstaged_open,
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_file(
        &self,
        file: &GitFile,
        side: Side,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let status = match side {
            Side::Staged => file.staged,
            Side::Unstaged => file.unstaged,
        }
        .unwrap_or(GitFileStatus::Modified);
        let (added, removed) = match side {
            Side::Staged => file.staged_stats,
            Side::Unstaged => file.unstaged_stats,
        };
        let display = self.display(&file.path);
        let (name, dir) = match display.rsplit_once('/') {
            Some((dir, name)) => (name.to_owned(), dir.to_owned()),
            None => (display.clone(), String::new()),
        };
        let prefix = match side {
            Side::Staged => "staged",
            Side::Unstaged => "unstaged",
        };
        let group: SharedString = format!("git-{prefix}-{index}").into();
        let open = file.clone();
        let toggle = file.repo_path.clone();
        let discard = file.clone();
        div()
            .id(group.clone())
            .group(group.clone())
            .debug_selector(move || format!("git-{prefix}-file-{index}"))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(ROW_HEIGHT))
            .cursor_pointer()
            .hover_style(|s| s.bg(colors.surface_hover))
            .child(status_badge(status, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_1p5()
                    .overflow_hidden()
                    .when(status == GitFileStatus::Deleted, |this| {
                        this.line_through().text_color(colors.text_muted)
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w_full()
                            .truncate()
                            .text_size(px(13.))
                            .child(name),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(dir),
                    ),
            )
            .when(
                side == Side::Unstaged && self.busy_label().is_none(),
                |this| {
                    this.child(
                        div()
                            .id(("discard", index))
                            .debug_selector(move || format!("git-discard-{index}"))
                            .invisible()
                            .group_hover_style(group, |s| s.visible())
                            .flex_shrink_0()
                            .p_0p5()
                            .rounded(px(3.))
                            .hover_style(|s| s.bg(colors.background))
                            .tooltip(tooltip("Discard changes"))
                            .child(
                                svg()
                                    .data(ICON_DISCARD)
                                    .size(px(12.))
                                    .text_color(colors.text_muted),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if this.busy.is_none() {
                                    this.discard(vec![discard.clone()], window, cx);
                                }
                            })),
                    )
                },
            )
            .when(added + removed > 0, |this| {
                this.child(stats(added, removed, cx))
            })
            .child(
                checkbox(
                    format!("git-{prefix}-check-{index}"),
                    side == Side::Staged,
                    self.control_state(false),
                    cx,
                )
                .debug_selector(move || format!("git-{prefix}-check-{index}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_staged(vec![toggle.clone()], side == Side::Unstaged, cx);
                })),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.open_file(&open, side, cx)))
            .into_any_element()
    }

    fn render_branch_bar(&self, status: &GitStatus, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let branch = status.branch.clone().unwrap_or_else(|| {
            status.head.as_deref().map_or_else(
                || "no branch".to_owned(),
                |h| format!("detached at {}", &h[..7.min(h.len())]),
            )
        });
        // The main remote action and its arrow.
        let remote: Option<RemoteButton> = match (&status.upstream, status.has_remote) {
            _ if status.branch.is_none() => None,
            (None, true) => Some((None, "Publish")),
            (None, false) => None,
            (Some(_), _) if status.behind > 0 => {
                Some((Some((ICON_ARROW_DOWN, status.behind)), "Pull"))
            }
            (Some(_), _) if status.ahead > 0 => Some((Some((ICON_ARROW_UP, status.ahead)), "Push")),
            (Some(_), _) => Some((None, "Fetch")),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .text_size(px(13.))
            .child(
                div()
                    .id("git-branch")
                    .debug_selector(|| "git-branch".into())
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .min_w_0()
                    .px_1()
                    .py_0p5()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover_style(|s| s.bg(colors.surface_hover))
                    .tooltip(tooltip("Switch or create a branch"))
                    .child(
                        svg()
                            .data(ICON_BRANCH)
                            .size(px(14.))
                            .flex_shrink_0()
                            .text_color(colors.text_muted),
                    )
                    .child(div().truncate().child(branch))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.branch_picker.is_some() {
                            this.close_branch_picker(cx);
                        } else {
                            this.open_branch_picker(window, cx);
                        }
                    })),
            )
            .child(div().flex_1())
            .when_some(remote, |this, (arrow, label)| {
                let content = div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .when_some(arrow, |this, (icon, count)| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_0p5()
                                .child(svg().data(icon).size(px(13.)).text_color(colors.text))
                                .child(count.to_string()),
                        )
                    })
                    .child(label);
                this.child(split_button(
                    "git-remote",
                    content,
                    self.control_state(false),
                    cx,
                    cx.listener(move |this, _, _, cx| match label {
                        "Push" | "Publish" => this.push(cx),
                        "Pull" => this.pull(cx),
                        _ => this.fetch(cx),
                    }),
                    cx.listener(|this, event: &ClickEvent, window, cx| {
                        this.remote_menu(event.position(), window, cx)
                    }),
                ))
            })
            .into_any_element()
    }

    /// The commit message (unless collapsed) with the commit controls under it.
    fn render_commit_box(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let nothing_to_commit = !self.options.amend && self.commit_files().is_empty();
        let generating = self.generating.is_some();
        let state = self.control_state(nothing_to_commit);
        let maximized = self.commit_maximized;
        let suggestion = self.suggested_message();
        let placeholder = if suggestion.is_empty() {
            "Enter commit message".to_owned()
        } else {
            suggestion
        };
        let icon_button = |id: &'static str, icon: &'static [u8], label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .p_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover_style(|s| s.bg(colors.surface_hover))
                .tooltip(tooltip(label))
                .child(svg().data(icon).size(px(14.)).text_color(colors.text_muted))
        };
        let editor = (!self.commit_collapsed).then(|| {
            div()
                .relative()
                .flex()
                .when(maximized, |this| this.flex_1().min_h_0())
                .child(
                    div()
                        .key_context("CommitMessage")
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .child(
                            text_area("git-commit-message")
                                .state(self.message.downgrade())
                                .placeholder(SharedString::from(placeholder))
                                .placeholder_color(rgb_to_hsla(colors.text_muted))
                                .caret_color(rgb_to_hsla(colors.text))
                                .selection_color(rgb_to_hsla(colors.accent).opacity(0.35))
                                .w_full()
                                .when(maximized, |this| this.h_full())
                                .when(!maximized, |this| this.min_h(px(96.)).max_h(px(220.)))
                                .overflow_y_scroll()
                                .pl_3()
                                .pr(px(36.))
                                .pt_2p5()
                                .pb_1()
                                .font_family(cx.theme().metrics.mono_font.clone())
                                .text_size(px(13.)),
                        ),
                )
                .child(
                    div().absolute().top(px(6.)).right(px(8.)).child(
                        icon_button(
                            "git-commit-maximize",
                            if maximized {
                                ICON_MINIMIZE
                            } else {
                                ICON_MAXIMIZE
                            },
                            if maximized {
                                "Restore the message editor"
                            } else {
                                "Expand the message editor"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.commit_maximized = !this.commit_maximized;
                            cx.notify();
                        })),
                    ),
                )
        });
        let collapsed = self.commit_collapsed;
        let options: Vec<&'static str> = [
            (self.options.amend, "Amend"),
            (self.options.signoff, "Signoff"),
            (self.options.no_verify, "Skip Hooks"),
        ]
        .into_iter()
        .filter_map(|(on, label)| on.then_some(label))
        .collect();
        div()
            .flex()
            .flex_col()
            .when(maximized && !collapsed, |this| this.flex_1().min_h_0())
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.background)
            .children(editor)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_2()
                    .pr_3()
                    .py_2()
                    .child(
                        icon_button(
                            "git-commit-collapse",
                            if collapsed {
                                ICON_CHEVRON_UP
                            } else {
                                ICON_CHEVRON
                            },
                            if collapsed {
                                "Show the message editor"
                            } else {
                                "Hide the message editor"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.commit_collapsed = !this.commit_collapsed;
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .when_some(self.busy_label(), |this, label| this.child(label))
                            .when(self.busy_label().is_none() && !options.is_empty(), |this| {
                                this.child(options.join(" · "))
                            }),
                    )
                    .child(
                        icon_button(
                            "git-generate-message",
                            ICON_STAR,
                            if generating {
                                "Stop writing the message"
                            } else {
                                "Write a commit message with the commit model"
                            },
                        )
                        .when(generating, |this| this.bg(colors.surface_hover))
                        .when(nothing_to_commit && !generating, |this| this.opacity(0.4))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if generating {
                                this.cancel_commit_message(cx);
                            } else if !nothing_to_commit {
                                this.generate_commit_message(cx);
                            }
                        })),
                    )
                    .child(split_button(
                        "git-commit",
                        div().child(self.commit_label()),
                        state,
                        cx,
                        cx.listener(|this, _, _, cx| this.commit(cx)),
                        cx.listener(|this, event: &ClickEvent, window, cx| {
                            this.commit_menu(event.position(), window, cx)
                        }),
                    )),
            )
            .into_any_element()
    }

    fn render_last_commit(&self, status: &GitStatus, cx: &mut Context<Self>) -> Option<AnyElement> {
        let summary = status.last_commit.clone()?;
        let colors = cx.theme().colors.clone();
        let busy = self.busy.is_some();
        let icon_button = |id: &'static str, icon: &'static [u8], label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .flex_shrink_0()
                .p_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover_style(|s| s.bg(colors.surface_hover))
                .tooltip(tooltip(label))
                .child(svg().data(icon).size(px(14.)).text_color(colors.text_muted))
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_1()
                .pl_3()
                .pr_2()
                .py_1()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .text_size(px(13.))
                .child(div().flex_1().min_w_0().truncate().child(summary))
                .when(self.busy_label().is_none(), |this| {
                    this.child(
                        icon_button(
                            "git-uncommit",
                            ICON_UNDO,
                            "Undo the last commit (its changes stay staged)",
                        )
                        .when(!busy, |this| {
                            this.on_click(
                                cx.listener(|this, _, window, cx| this.uncommit(window, cx)),
                            )
                        }),
                    )
                })
                .child(
                    icon_button("git-show-last-commit", ICON_COMMIT, "Show the last commit")
                        .on_click(cx.listener(|this, _, _, cx| this.open_last_commit(cx))),
                )
                .into_any_element(),
        )
    }

    /// The history: a virtual list, so only the commits in view are built and laid out
    /// (a long history drawn whole slowed every frame of the window).
    fn render_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(history) = &self.history else {
            return message("Loading…", cx);
        };
        if history.is_empty() {
            return message("No commits yet.", cx);
        }
        uniform_list(
            "git-history",
            history.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                let ahead = this.status().map_or(0, |s| s.ahead);
                let has_upstream = this.status().is_some_and(|s| s.upstream.is_some());
                let now = zp_ai::now_ms();
                let commits = this.history.as_deref().unwrap_or_default();
                range
                    .filter_map(|index| {
                        let commit = commits.get(index)?;
                        let unpushed = has_upstream && index < ahead;
                        Some(this.render_commit_row(index, commit, unpushed, now, cx))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.history_scroll)
        // A set width: otherwise the list can take the width of a row measured untruncated.
        .w_full()
        .flex_1()
        .min_h_0()
        .py_1()
        .into_any_element()
    }

    fn render_commit_row(
        &self,
        index: usize,
        commit: &Commit,
        unpushed: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let open = commit.clone();
        div()
            .id(("git-commit-row", index))
            .debug_selector(move || format!("git-commit-{index}"))
            // Rows are laid out on their own: without a set width a row grows to its
            // untruncated text, and a long commit summary runs past the panel.
            .w_full()
            .flex()
            .flex_col()
            .px_3()
            .py_1()
            .cursor_pointer()
            .hover_style(|s| s.bg(colors.surface_hover))
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .items_baseline()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(commit.summary.clone()),
                    )
                    .when(unpushed, |this| {
                        this.child(
                            div()
                                .id(("git-unpushed", index))
                                .flex_shrink_0()
                                .text_size(px(11.))
                                .text_color(colors.accent)
                                .child("↑")
                                .tooltip(tooltip("Not pushed yet")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .text_size(px(11.))
                    .text_color(colors.text_muted)
                    .child(
                        div()
                            .font_family(cx.theme().metrics.mono_font.clone())
                            .child(commit.short_sha.clone()),
                    )
                    .child(div().min_w_0().truncate().child(format!(
                        "{} · {}",
                        commit.author,
                        crate::ui::relative_time(commit.time * 1000, now)
                    )))
                    .when(!commit.refs.is_empty(), |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(colors.accent)
                                .child(commit.refs.clone()),
                        )
                    }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.open_commit(&open, cx)))
            .into_any_element()
    }

    fn render_branch_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.branch_picker.as_ref()?;
        let colors = cx.theme().colors.clone();
        let (matches, create) = self.picker_choices(cx);
        let loading = picker.branches.is_none();
        let rows = matches.into_iter().enumerate().map(|(index, branch)| {
            let name = branch.name.clone();
            let current = branch.current;
            let remote = branch.remote;
            let summary = branch.summary.clone();
            div()
                .id(("git-branch-item", index))
                .debug_selector(move || format!("git-branch-item-{index}"))
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(
                    div()
                        .flex()
                        .gap_1p5()
                        .child(div().w(px(12.)).child(if current { "✓" } else { "" }))
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(remote, |this| {
                            this.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(colors.text_muted)
                                    .child("remote"),
                            )
                        }),
                )
                .child(
                    div()
                        .pl(px(18.))
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .truncate()
                        .child(summary),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.switch_branch(branch.clone(), cx)))
        });
        let create_row = create.map(|name| {
            div()
                .id("git-branch-create")
                .px_2()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(colors.accent)
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(format!("+ Create branch “{name}”"))
                .on_click(cx.listener(move |this, _, _, cx| this.create_branch(name.clone(), cx)))
        });
        Some(
            div()
                .occlude()
                .key_context("BranchPicker")
                .mx_2()
                .p_1()
                .flex()
                .flex_col()
                .gap_0p5()
                .max_h(px(320.))
                .bg(colors.surface)
                .border_1()
                .border_color(colors.border)
                .rounded(px(6.))
                .shadow_lg()
                .text_size(px(12.))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_branch_picker(cx)))
                .child(
                    text_input("git-branch-filter")
                        .state(picker.filter.downgrade())
                        .placeholder("Switch to or create a branch…")
                        .placeholder_color(rgb_to_hsla(colors.text_muted))
                        .caret_color(rgb_to_hsla(colors.text))
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.background)
                        .whitespace_nowrap(),
                )
                .child(
                    div()
                        .id("git-branch-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .when(loading, |this| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .text_color(colors.text_muted)
                                    .child("Loading…"),
                            )
                        })
                        .children(create_row)
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}

impl Render for GitPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let status = match &self.status {
            Load::Loading => return message("Loading…", cx),
            Load::NotRepository => return message("Not a git repository.", cx),
            Load::Failed(error) => return message(error, cx),
            Load::Loaded(status) => status.clone(),
        };
        let maximized =
            self.commit_maximized && !self.commit_collapsed && self.tab == GitTab::Changes;
        let body = match self.tab {
            _ if maximized => div().into_any_element(),
            GitTab::Changes => self.render_changes(&status, cx),
            GitTab::History => self.render_history(cx),
        };
        let picker = self.render_branch_picker(cx);
        let last_commit = self.render_last_commit(&status, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("GitPanel")
            .on_action(
                cx.listener(|this, _: &crate::actions::CommitChanges, _, cx| this.commit(cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::ConfirmBranch, _, cx| {
                    this.confirm_branch(cx)
                }),
            )
            .on_action(cx.listener(|this, _: &crate::actions::ClosePicker, _, cx| {
                this.close_branch_picker(cx)
            }))
            .child(self.render_tabs(cx))
            .child(body)
            .when_some(self.notice.clone(), |this, (text, error)| {
                this.child(
                    div()
                        .id("git-notice")
                        .px_3()
                        .py_1()
                        .text_size(px(12.))
                        .text_color(if error {
                            colors.error
                        } else {
                            colors.text_muted
                        })
                        .cursor_pointer()
                        .child(text)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        })),
                )
            })
            .children(picker)
            .child(self.render_branch_bar(&status, cx))
            .when(self.tab == GitTab::Changes, |this| {
                this.child(self.render_commit_box(cx))
            })
            .children(last_commit)
            .when_some(self.menu.as_ref(), |this, open| {
                this.child(
                    deferred(
                        anchored()
                            .position(open.position)
                            .snap_to_window_with_margin(px(8.))
                            .child(open.menu.clone()),
                    )
                    .priority(2),
                )
            })
            .into_any_element()
    }
}

fn file_name(repo_path: &str) -> &str {
    repo_path.rsplit('/').next().unwrap_or(repo_path)
}

fn message(text: &str, cx: &App) -> AnyElement {
    div()
        .px_3()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().colors.text_muted)
        .child(text.to_owned())
        .into_any_element()
}

fn stats(added: usize, removed: usize, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .flex_shrink_0()
        .gap_1()
        .text_size(px(11.))
        .child(div().text_color(colors.success).child(format!("+{added}")))
        .child(div().text_color(colors.error).child(format!("−{removed}")))
}

/// The status icon: a rounded square with a dot (modified), plus (new), minus
/// (deleted), arrow (renamed) or exclamation mark (conflict), in the status color.
fn status_badge(status: GitFileStatus, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    let color: Rgba = match status {
        GitFileStatus::Added | GitFileStatus::Untracked => colors.success,
        GitFileStatus::Deleted | GitFileStatus::Conflicted => colors.error,
        GitFileStatus::Modified | GitFileStatus::Renamed => colors.warning,
    };
    let bar = |w: f32, h: f32| div().w(px(w)).h(px(h)).rounded(px(0.5)).bg(color);
    let mark = match status {
        GitFileStatus::Modified => div()
            .size(px(4.))
            .rounded_full()
            .bg(color)
            .into_any_element(),
        GitFileStatus::Added | GitFileStatus::Untracked => div()
            .relative()
            .size(px(7.))
            .child(bar(7., 1.5).absolute().top(px(2.75)))
            .child(bar(1.5, 7.).absolute().left(px(2.75)))
            .into_any_element(),
        GitFileStatus::Deleted => bar(7., 1.5).into_any_element(),
        GitFileStatus::Renamed => div()
            .text_size(px(9.))
            .text_color(color)
            .child("→")
            .into_any_element(),
        GitFileStatus::Conflicted => div()
            .text_size(px(9.))
            .font_weight(FontWeight::BOLD)
            .text_color(color)
            .child("!")
            .into_any_element(),
    };
    div()
        .size(px(13.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .border_1()
        .border_color(color)
        .child(mark)
}

/// A drawn checkbox: dark when off, a check mark in the accent color when on.
fn checkbox(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    state: ControlState,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = &cx.theme().colors;
    div()
        .id(id)
        .size(px(16.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(if checked {
            colors.accent.opacity(0.7)
        } else {
            colors.border
        })
        .bg(colors.background)
        .when(checked, |this| {
            this.child(
                svg()
                    .data(ICON_CHECK)
                    .size(px(11.))
                    .text_color(colors.accent),
            )
        })
        .when(state.dimmed, |this| this.opacity(0.5))
        .when(!state.dimmed, |this| {
            let hover = colors.text_muted;
            this.cursor_pointer()
                .hover_style(move |s| s.border_color(hover))
        })
}

/// A button with a main action and a chevron opening more.
fn split_button(
    id: &'static str,
    content: impl IntoElement,
    state: ControlState,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_more: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let colors = cx.theme().colors.clone();
    let part = |id: SharedString| {
        div()
            .id(id)
            .h_full()
            .flex()
            .items_center()
            .when(!state.dimmed, |this| {
                this.cursor_pointer().hover_style(|s| s.bg(colors.border))
            })
    };
    div()
        .h(px(24.))
        .flex()
        .flex_shrink_0()
        .overflow_hidden()
        .rounded(px(5.))
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface_hover)
        .text_size(px(13.))
        .text_color(colors.text)
        .when(state.dimmed, |this| this.opacity(0.5))
        .child(
            part(id.into())
                .debug_selector(move || id.to_owned())
                .px_2()
                .child(content)
                .when(!state.disabled, |this| this.on_click(on_click)),
        )
        .child(
            part(format!("{id}-more").into())
                .debug_selector(move || format!("{id}-more"))
                .px_1()
                .border_l_1()
                .border_color(colors.border)
                .child(
                    svg()
                        .data(ICON_CHEVRON)
                        .size(px(12.))
                        .text_color(colors.text_muted),
                )
                .when(!state.disabled, |this| this.on_click(on_more)),
        )
}
