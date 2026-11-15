//! The window of one project: sessions sidebar, conversation tabs, changes panel and
//! status bar.

mod sidebar;
mod tabs;

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, ColorExt as _, Context, DismissEvent, DragMoveEvent, Entity, FocusHandle,
    Focusable, FontWeight, IntoElement, Pixels, Point, Render, Subscription, Window, anchored,
    deferred, div, prelude::*, px, rgb_to_hsla,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, TextChanged, text_input};
use zp_core::app_state::PanelLayout;
use zp_session::{Session, SessionManager};

use crate::actions::{
    ActivateTab, CancelRename, ClearSessionSearch, ConfirmRename, NewChat, NextTab,
    OpenSelectedSession, PROJECT_CONTEXT, PreviousTab, SelectNextSession, SelectPreviousSession,
    ToggleRightPanel, ToggleSessionTree, ToggleSidebar,
};
use crate::changes_panel::{ChangesMode, ChangesPanel, ChangesPanelEvent};
use crate::context_menu::ContextMenu;
use crate::diff_view::{DiffItem, DiffLayout, DiffView, DiffViewEvent};
use crate::store::{AppPaths, AppStore};
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{CUSTOM_TITLE_BAR, title_bar};
use crate::workspace::{self, Workspaces};

pub use tabs::Tab;

const SIDEBAR_WIDTH: (f32, f32) = (160.0, 480.0);
const RIGHT_PANEL_WIDTH: (f32, f32) = (220.0, 640.0);
/// The conversation column never gets narrower than this when resizing panels.
const CENTER_MIN_WIDTH: f32 = 320.0;
const RESIZE_HANDLE_WIDTH: f32 = 6.0;

/// Which panel a resize drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelEdge {
    Sidebar,
    Right,
}

/// Drag payload of a panel resize handle.
#[derive(Clone, Copy, Debug)]
struct PanelResize(PanelEdge);

/// Invisible drag preview for resize handles.
struct NoPreview;

impl Render for NoPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// What a rename applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameTarget {
    /// The session of a tab (by its conversation).
    Tab(gpui::EntityId),
    /// A session file not open in any tab.
    Session(PathBuf),
}

struct Rename {
    target: RenameTarget,
    input: Entity<EditableTextState>,
    position: Point<Pixels>,
}

struct OpenDiff {
    view: Entity<DiffView>,
    _events: Subscription,
}

struct OpenMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _dismiss: Subscription,
}

/// Root view of a project window.
pub struct ProjectWindow {
    root: PathBuf,
    name: String,
    branch: Option<String>,
    layout: PanelLayout,
    tabs: Vec<Tab>,
    active: usize,
    sidebar: sidebar::Sidebar,
    menu: Option<OpenMenu>,
    rename: Option<Rename>,
    changes: Entity<ChangesPanel>,
    /// A diff shown instead of the active conversation.
    diff: Option<OpenDiff>,
    diff_layout: DiffLayout,
    /// Whether the trust question is on screen.
    trust_prompt_open: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ProjectWindow {
    /// The window title for a project: `name — branch`.
    pub fn title_for(root: &Path) -> String {
        let name = zp_core::project::project_name(root);
        match zp_core::project::git_branch(root) {
            Some(branch) => format!("{name} — {branch}"),
            None => name,
        }
    }

    /// A window restoring the project's tabs.
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty(root, window, cx);
        this.restore_tabs(window, cx);
        this.check_trust(window, cx);
        this
    }

    // ----- permissions and trust -----------------------------------------------------

    /// Applies the project's current permission policy and trust to every tab.
    pub(crate) fn apply_permissions(&mut self, cx: &mut Context<Self>) {
        let policy = crate::approvals::project_policy(&self.root, cx);
        let trusted = crate::approvals::is_trusted(&self.root, cx);
        for tab in &self.tabs {
            tab.conversation.update(cx, |view, cx| {
                view.set_permission_policy(policy.clone(), cx);
                view.set_trusted(trusted);
            });
        }
        cx.notify();
    }

    /// Asks whether to trust the project the first time it has instructions or settings.
    fn check_trust(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if AppStore::state(cx).project(&self.root).trusted.is_none()
            && !zp_core::project::trust_files(&self.root).is_empty()
        {
            self.prompt_trust(window, cx);
        }
    }

    /// Whether the project's instructions and settings are ignored.
    fn is_restricted(&self, cx: &App) -> bool {
        AppStore::state(cx).project(&self.root).trusted != Some(true)
            && !zp_core::project::trust_files(&self.root).is_empty()
    }

    /// Asks the user whether to trust the project and applies the answer.
    pub(crate) fn prompt_trust(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.trust_prompt_open {
            return;
        }
        let files: Vec<String> = zp_core::project::trust_files(&self.root)
            .iter()
            .map(|file| {
                file.strip_prefix(&self.root)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| zp_core::project::tilde_path(file))
            })
            .collect();
        let detail = format!(
            "This folder has instructions or settings for AI agents:\n{}\n\nA trusted project's \
             files can tell the agent what to do and change its permissions. If you don't \
             trust the folder's authors, ZP ignores them.",
            files.join("\n")
        );
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Do you trust this project?",
            Some(&detail),
            &["Trust Project", "Don't Trust"],
            cx,
        );
        self.trust_prompt_open = true;
        cx.spawn_in(window, async move |this, cx| {
            let trusted = answer.await.ok().map(|answer| answer == 0);
            let _ = this.update(cx, |this, cx| {
                this.trust_prompt_open = false;
                if let Some(trusted) = trusted {
                    let root = this.root.clone();
                    AppStore::update(cx, move |state| {
                        state.project_mut(&root).trusted = Some(trusted);
                    });
                    this.apply_permissions(cx);
                }
            });
        })
        .detach();
    }

    /// A window with one tab showing `session`.
    #[cfg(feature = "visual-tests")]
    pub fn new_with_session(
        root: PathBuf,
        session: Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::empty(root, window, cx);
        this.add_tab(session, false, window, cx);
        this
    }

    fn empty(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let layout = AppStore::state(cx).project(&root).layout;

        let geometry_root = root.clone();
        let release_root = root.clone();
        let search = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let changes = cx.new(|_| ChangesPanel::new(root.clone()));
        let subscriptions = vec![
            cx.subscribe_in(&changes, window, |this, _, event, window, cx| match event {
                ChangesPanelEvent::OpenDiff { items, index } => {
                    this.open_diff(items.clone(), *index, window, cx)
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                if Theme::sync_with_appearance(window.appearance(), cx) {
                    cx.refresh_windows();
                }
            }),
            cx.observe_window_bounds(window, move |_, window, cx| {
                let geometry = workspace::capture_geometry(window, cx);
                AppStore::update(cx, |state| {
                    state.project_mut(&geometry_root).window = Some(geometry);
                });
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.refresh(window, cx);
                }
            }),
            cx.subscribe(&search, |this, _, _: &TextChanged, cx| {
                this.sidebar.on_search_changed();
                cx.notify();
            }),
            cx.on_release(move |_, cx| Workspaces::project_closed(&release_root, cx)),
        ];

        let mut this = Self {
            name: zp_core::project::project_name(&root),
            branch: None,
            root,
            layout,
            tabs: Vec::new(),
            active: 0,
            sidebar: sidebar::Sidebar::new(search),
            menu: None,
            rename: None,
            changes,
            diff: None,
            diff_layout: DiffLayout::Unified,
            trust_prompt_open: false,
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.refresh(window, cx);
        this
    }

    /// The project directory.
    #[cfg(test)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The current panel layout.
    #[cfg(test)]
    pub fn layout(&self) -> &PanelLayout {
        &self.layout
    }

    /// Re-reads the branch (updating the title) and the session list.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let branch = zp_core::project::git_branch(&self.root);
        if branch != self.branch || self.branch.is_none() {
            self.branch = branch;
            window.set_window_title(&Self::title_for(&self.root));
        }
        self.load_sessions(cx);
        if self.changes.read(cx).mode() == ChangesMode::Git {
            self.changes.update(cx, |panel, cx| panel.refresh_git(cx));
        }
        cx.notify();
    }

    /// The changes panel (tests).
    #[cfg(any(test, feature = "visual-tests"))]
    pub fn changes_panel(&self) -> &Entity<ChangesPanel> {
        &self.changes
    }

    /// The diff shown instead of the conversation (tests).
    #[cfg(test)]
    pub fn diff_view(&self) -> Option<Entity<DiffView>> {
        self.diff.as_ref().map(|diff| diff.view.clone())
    }

    /// Shows `items` in a diff view over the conversation.
    pub(crate) fn open_diff(
        &mut self,
        items: Vec<DiffItem>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = self.diff_layout;
        let view = cx.new(|cx| DiffView::new(items, index, layout, cx));
        let events = cx.subscribe_in(&view, window, |this, view, event, window, cx| match event {
            DiffViewEvent::Close => {
                this.diff_layout = view.read(cx).layout();
                this.close_diff(window, cx);
            }
        });
        view.read(cx).focus_handle(cx).focus(window, cx);
        self.diff = Some(OpenDiff {
            view,
            _events: events,
        });
        cx.notify();
    }

    pub(crate) fn close_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(diff) = self.diff.take() {
            self.diff_layout = diff.view.read(cx).layout();
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn save_layout(&self, cx: &mut App) {
        let root = self.root.clone();
        let layout = self.layout.clone();
        AppStore::update(cx, move |state| state.project_mut(&root).layout = layout);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.layout.sidebar_visible = !self.layout.sidebar_visible;
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_right_panel(&mut self, _: &ToggleRightPanel, _: &mut Window, cx: &mut Context<Self>) {
        self.layout.right_panel_visible = !self.layout.right_panel_visible;
        self.save_layout(cx);
        cx.notify();
    }

    fn toggle_session_tree(
        &mut self,
        _: &ToggleSessionTree,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(conversation) = self.active_conversation() {
            conversation.update(cx, |view, cx| view.toggle_tree(cx));
        }
    }

    fn on_resize_drag(
        &mut self,
        event: &DragMoveEvent<PanelResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PanelResize(edge) = *event.drag(cx);
        resize_layout(
            &mut self.layout,
            edge,
            event.event.position.x.as_f32(),
            window.viewport_size().width.as_f32(),
        );
        self.save_layout(cx);
        cx.notify();
    }

    // ----- menus and rename ----------------------------------------------------------

    /// Shows `menu` at `position` (window coordinates).
    fn show_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
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

    /// The open context menu (tests).
    #[cfg(test)]
    pub fn context_menu(&self) -> Option<Entity<ContextMenu>> {
        self.menu.as_ref().map(|open| open.menu.clone())
    }

    /// Starts renaming `target`, prefilled with `current`.
    fn start_rename(
        &mut self,
        target: RenameTarget,
        current: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = current.to_owned();
        let input = cx.new(|cx| EditableTextState::new(StringStorage::from(current), cx));
        input.update(cx, |input, cx| input.select_document(cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.rename = Some(Rename {
            target,
            input,
            position,
        });
        cx.notify();
    }

    /// The rename in progress (tests): its target and input.
    #[cfg(test)]
    pub fn rename_input(&self) -> Option<(RenameTarget, Entity<EditableTextState>)> {
        self.rename
            .as_ref()
            .map(|rename| (rename.target.clone(), rename.input.clone()))
    }

    pub(crate) fn confirm_rename(
        &mut self,
        _: &ConfirmRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let name = rename.input.read(cx).as_str().trim().to_owned();
        let name = (!name.is_empty()).then_some(name);
        let result = match rename.target {
            RenameTarget::Tab(id) => {
                match self.tabs.iter().find(|t| t.conversation.entity_id() == id) {
                    Some(tab) => tab
                        .conversation
                        .update(cx, |view, cx| view.rename(name, cx)),
                    None => Ok(()),
                }
            }
            RenameTarget::Session(path) => match self.tab_for_file(&path, cx) {
                Some(index) => {
                    let conversation = self.tabs[index].conversation.clone();
                    conversation.update(cx, |view, cx| view.rename(name, cx))
                }
                None => Session::open(&path)
                    .and_then(|mut session| session.append_session_info(name).map(|_| ())),
            },
        };
        if let Err(error) = result {
            workspace::report_error("Could not rename the session", &error, cx);
        }
        self.load_sessions(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn cancel_rename(&mut self, _: &CancelRename, window: &mut Window, cx: &mut Context<Self>) {
        self.rename = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        match self.active_conversation() {
            Some(conversation) => {
                let focus = conversation.read(cx).composer_focus(cx);
                focus.focus(window, cx);
            }
            None => self.focus_handle.focus(window, cx),
        }
    }

    // ----- rendering ---------------------------------------------------------------

    fn resize_handle(&self, edge: PanelEdge, cx: &App) -> impl IntoElement {
        let accent = cx.theme().colors.accent;
        div()
            .id(match edge {
                PanelEdge::Sidebar => "resize-sidebar",
                PanelEdge::Right => "resize-right-panel",
            })
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex_shrink_0()
            .cursor_col_resize()
            .hover_style(move |style| style.bg(accent.opacity(0.35)))
            .on_drag(PanelResize(edge), |_, _, _, cx| cx.new(|_| NoPreview))
    }

    fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("center")
            .flex_1()
            .min_w(px(CENTER_MIN_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_tab_bar(cx))
            .child(match &self.diff {
                Some(diff) => div().flex_1().min_h_0().child(diff.view.clone()),
                None => div()
                    .flex_1()
                    .min_h_0()
                    .children(self.active_conversation().cloned()),
            })
    }

    fn render_right_panel(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .id("right-panel")
            .w(px(self.layout.right_panel_width))
            .h_full()
            .flex_shrink_0()
            .bg(colors.surface)
            .border_l_1()
            .border_color(colors.border)
            .child(self.changes.clone())
    }

    fn render_title_bar(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        title_bar("title-bar")
            .gap_2()
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.name.clone()),
            )
            .when_some(self.branch.clone(), |this, branch| {
                this.child(
                    div()
                        .text_color(colors.text_muted)
                        .child(format!("⎇ {branch}")),
                )
            })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let status = self.active_conversation().map(|c| c.read(cx).status());
        let running_tabs = self
            .tabs
            .iter()
            .filter(|tab| tab.conversation.read(cx).is_running())
            .count();
        let changed_files = self.changes.read(cx).file_count();
        let waiting = self
            .tabs
            .iter()
            .filter(|tab| tab.conversation.read(cx).needs_attention())
            .count();
        let restricted = self.is_restricted(cx);
        let mut right = Vec::new();
        if waiting > 0 {
            right.push(format!(
                "{waiting} tab{} awaiting approval",
                if waiting == 1 { "" } else { "s" }
            ));
        }
        if running_tabs > 1 {
            right.push(format!("{running_tabs} tabs running"));
        } else if status.as_ref().is_some_and(|s| s.running) {
            right.push("running".to_owned());
        }
        match status
            .as_ref()
            .and_then(|s| s.model.clone().map(|m| (s, m)))
        {
            None => right.push("No model configured".to_owned()),
            Some((status, model)) => {
                right.push(model.qualified_id());
                if model.reasoning {
                    right.push(format!(
                        "thinking {}",
                        model.clamp_thinking_level(status.thinking)
                    ));
                }
                let usage = &status.usage;
                let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
                right.push(format!("{} tokens", format_tokens(tokens)));
                right.push(format!("${:.4}", usage.cost));
                if model.context_window > 0 {
                    right.push(format!(
                        "context {:.0}%",
                        usage.last_context_tokens as f64 * 100.0 / model.context_window as f64
                    ));
                }
            }
        }
        div()
            .id("status-bar")
            .h(px(24.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .text_size(px(12.))
            .text_color(colors.text_muted)
            .bg(colors.surface)
            .border_t_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        self.branch
                            .as_ref()
                            .map(|branch| format!("⎇ {branch}"))
                            .unwrap_or_default(),
                    )
                    .when(restricted, |this| {
                        this.child(
                            div()
                                .id("status-restricted")
                                .cursor_pointer()
                                .text_color(colors.warning)
                                .hover_style(|s| s.text_color(colors.text))
                                .child("Restricted Mode")
                                .tooltip(crate::ui::tooltip(
                                    "The project's instructions and settings are ignored. Click to review.",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.prompt_trust(window, cx)
                                })),
                        )
                    })
                    .when(changed_files > 0, |this| {
                        this.child(
                            div()
                                .id("status-changes")
                                .cursor_pointer()
                                .hover_style(|s| s.text_color(colors.text))
                                .child(format!(
                                    "{changed_files} file{} changed",
                                    if changed_files == 1 { "" } else { "s" }
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_right_panel(&ToggleRightPanel, window, cx)
                                })),
                        )
                    }),
            )
            .child(div().child(right.join("  ·  ")))
    }

    fn render_overlays(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = cx.theme().colors.clone();
        let mut overlays = Vec::new();
        if let Some(open) = &self.menu {
            overlays.push(
                deferred(
                    anchored()
                        .position(open.position)
                        .snap_to_window_with_margin(px(8.))
                        .child(open.menu.clone()),
                )
                .priority(2)
                .into_any_element(),
            );
        }
        if let Some(rename) = &self.rename {
            overlays.push(
                deferred(
                    anchored()
                        .position(rename.position)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            div()
                                .occlude()
                                .w(px(300.))
                                .p_2()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .bg(colors.surface)
                                .border_1()
                                .border_color(colors.border)
                                .rounded(px(6.))
                                .shadow_lg()
                                .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                                    this.cancel_rename(&CancelRename, window, cx)
                                }))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(colors.text_muted)
                                        .child("Rename session — Enter to save, Esc to cancel"),
                                )
                                .child(
                                    div().key_context("RenameInput").child(
                                        text_input("rename-input")
                                            .state(rename.input.downgrade())
                                            .caret_color(rgb_to_hsla(colors.text))
                                            .px_2()
                                            .py_1()
                                            .w_full()
                                            .rounded(px(4.))
                                            .border_1()
                                            .border_color(colors.accent)
                                            .whitespace_nowrap(),
                                    ),
                                ),
                        ),
                )
                .priority(2)
                .into_any_element(),
            );
        }
        overlays
    }
}

/// `1234` → `1.2k`, `2500000` → `2.5M`.
fn format_tokens(tokens: u64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1_000.0),
        t => t.to_string(),
    }
}

/// The session manager of the data directory, if any.
fn session_manager(cx: &App) -> Option<SessionManager> {
    AppPaths::sessions_dir(cx).map(SessionManager::new)
}

/// A new session for `root`, saved under the data directory (in memory without one).
fn new_session(root: &Path, cx: &App) -> Session {
    match session_manager(cx) {
        Some(manager) => manager.create(root),
        None => Session::in_memory(root),
    }
}

/// Sets a panel width from the pointer position (in window coordinates), keeping the
/// center column usable.
pub fn resize_layout(layout: &mut PanelLayout, edge: PanelEdge, pointer_x: f32, window_width: f32) {
    match edge {
        PanelEdge::Sidebar => {
            let right = if layout.right_panel_visible {
                layout.right_panel_width
            } else {
                0.0
            };
            let max = (window_width - right - CENTER_MIN_WIDTH).min(SIDEBAR_WIDTH.1);
            layout.sidebar_width = pointer_x.min(max).max(SIDEBAR_WIDTH.0);
        }
        PanelEdge::Right => {
            let left = if layout.sidebar_visible {
                layout.sidebar_width
            } else {
                0.0
            };
            let max = (window_width - left - CENTER_MIN_WIDTH).min(RIGHT_PANEL_WIDTH.1);
            layout.right_panel_width = (window_width - pointer_x).min(max).max(RIGHT_PANEL_WIDTH.0);
        }
    }
}

impl Focusable for ProjectWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ProjectWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let font_size = theme.metrics.font_size;
        div()
            .id("project-window")
            .key_context(PROJECT_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_right_panel))
            .on_action(cx.listener(Self::toggle_session_tree))
            .on_action(cx.listener(|this, _: &NewChat, window, cx| this.new_tab(window, cx)))
            .on_action(cx.listener(|this, action: &ActivateTab, window, cx| {
                let index = action.0.min(this.tabs.len().saturating_sub(1));
                this.activate_tab(index, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousTab, window, cx| this.cycle_tab(-1, window, cx)),
            )
            .on_action(cx.listener(Self::close_active_tab))
            .on_action(cx.listener(|this, _: &SelectNextSession, window, cx| {
                this.move_session_selection(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousSession, window, cx| {
                this.move_session_selection(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenSelectedSession, window, cx| {
                this.open_selected_session(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClearSessionSearch, window, cx| {
                this.clear_session_search(window, cx)
            }))
            .on_action(cx.listener(Self::confirm_rename))
            .on_action(cx.listener(Self::cancel_rename))
            .on_drag_move(cx.listener(Self::on_resize_drag))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .text_size(font_size)
            .when(CUSTOM_TITLE_BAR, |this| {
                this.child(self.render_title_bar(cx))
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .when(self.layout.sidebar_visible, |row| {
                        row.child(self.render_sidebar(cx))
                            .child(self.resize_handle(PanelEdge::Sidebar, cx))
                    })
                    .child(self.render_center(cx))
                    .when(self.layout.right_panel_visible, |row| {
                        row.child(self.resize_handle(PanelEdge::Right, cx))
                            .child(self.render_right_panel(cx))
                    }),
            )
            .child(self.render_status_bar(cx))
            .children(self.render_overlays(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_resizing_is_clamped() {
        let mut layout = PanelLayout {
            right_panel_visible: true,
            ..PanelLayout::default()
        };
        resize_layout(&mut layout, PanelEdge::Sidebar, 300.0, 1400.0);
        assert_eq!(layout.sidebar_width, 300.0);
        resize_layout(&mut layout, PanelEdge::Sidebar, 20.0, 1400.0);
        assert_eq!(layout.sidebar_width, SIDEBAR_WIDTH.0);
        resize_layout(&mut layout, PanelEdge::Sidebar, 2000.0, 1400.0);
        assert_eq!(layout.sidebar_width, SIDEBAR_WIDTH.1);
        // The right panel is measured from the right edge.
        resize_layout(&mut layout, PanelEdge::Right, 1000.0, 1400.0);
        assert_eq!(layout.right_panel_width, 400.0);
        // The center keeps its minimum width in a narrow window.
        resize_layout(&mut layout, PanelEdge::Right, 100.0, 1100.0);
        assert_eq!(
            layout.right_panel_width,
            1100.0 - SIDEBAR_WIDTH.1 - CENTER_MIN_WIDTH
        );
        // In a window too narrow for both, panels keep their minimum width.
        resize_layout(&mut layout, PanelEdge::Right, 100.0, 900.0);
        assert_eq!(layout.right_panel_width, RIGHT_PANEL_WIDTH.0);
    }
}
