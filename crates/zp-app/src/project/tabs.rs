//! Conversation tabs: each tab is an independent conversation with its own agent, so
//! several can run at once (background tabs keep running).

use std::path::Path;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, Entity, IntoElement, MouseButton,
    PromptLevel, Render, SharedString, Subscription, Window, div, prelude::*, px,
};
use zp_core::app_state::TabState;
use zp_session::Session;

use super::{ProjectWindow, RenameTarget, new_session};
use crate::actions::CloseTab;
use crate::context_menu::ContextMenu;
use crate::conversation::{ConversationEvent, ConversationView};
use crate::store::AppStore;
use crate::theme::ActiveTheme;

/// One conversation tab.
pub struct Tab {
    pub conversation: Entity<ConversationView>,
    /// Pinned tabs stay at the front.
    pub pinned: bool,
    /// A preview tab (single click in the sidebar) is replaced by the next preview.
    pub preview: bool,
    /// Finished in the background since it was last shown.
    pub unread: bool,
    _events: Subscription,
}

/// Drag payload for reordering tabs.
#[derive(Clone)]
struct DraggedTab {
    index: usize,
    title: SharedString,
}

struct TabGhost(SharedString);

impl Render for TabGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = &cx.theme().colors;
        div()
            .px_3()
            .py_1()
            .rounded(px(4.))
            .bg(colors.surface_hover)
            .border_1()
            .border_color(colors.accent)
            .text_color(colors.text)
            .child(self.0.clone())
    }
}

impl ProjectWindow {
    #[cfg(feature = "visual-tests")]
    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    /// The tabs (tests).
    #[cfg(test)]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// Index of the active tab.
    #[cfg(test)]
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The conversation of the active tab.
    pub fn active_conversation(&self) -> Option<&Entity<ConversationView>> {
        self.tabs.get(self.active).map(|tab| &tab.conversation)
    }

    /// The tab showing session file `path`.
    pub(crate) fn tab_for_file(&self, path: &Path, cx: &App) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.conversation.read(cx).session_file().as_deref() == Some(path))
    }

    fn index_of(&self, conversation: &Entity<ConversationView>) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| &tab.conversation == conversation)
    }

    fn make_tab(
        &mut self,
        session: Session,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
        let root = self.root.clone();
        let conversation = cx.new(|cx| ConversationView::new(root, session, window, cx));
        let events = cx.subscribe_in(
            &conversation,
            window,
            |this, conversation, event, window, cx| {
                let Some(index) = this.index_of(conversation) else {
                    return;
                };
                match event {
                    ConversationEvent::StatusChanged => {}
                    ConversationEvent::PermissionModeChanged(mode) => {
                        let (root, mode) = (this.root.clone(), *mode);
                        AppStore::update(cx, move |state| {
                            state.project_mut(&root).permission_mode = Some(mode);
                        });
                        this.apply_permissions(cx);
                    }
                    ConversationEvent::RunFinished => {
                        if index != this.active {
                            this.tabs[index].unread = true;
                        }
                        let view = conversation.read(cx);
                        let body = match view.failure() {
                            Some(error) => format!("Failed: {}", zp_tools::shorten(&error, 120)),
                            None => "Finished".to_owned(),
                        };
                        this.notify_background(conversation, body, window, cx);
                        this.save_tabs(cx);
                        this.load_sessions(cx);
                    }
                    ConversationEvent::TitleChanged => this.load_sessions(cx),
                    ConversationEvent::Activity => {
                        if this.tabs[index].preview {
                            this.tabs[index].preview = false;
                            this.save_tabs(cx);
                        }
                    }
                    ConversationEvent::Forked(handoff) => {
                        if let Some(session) = handoff.borrow_mut().take() {
                            this.add_tab(session, false, window, cx);
                            this.load_sessions(cx);
                        }
                    }
                    ConversationEvent::FilesChanged => {}
                    ConversationEvent::FilesRestored => {
                        if index == this.active {
                            this.close_diff(window, cx);
                        }
                        this.save_tabs(cx);
                        this.load_sessions(cx);
                    }
                    ConversationEvent::ApprovalRequested(summary) => {
                        let body = format!("Needs approval: {}", zp_tools::shorten(summary, 120));
                        this.notify_background(conversation, body, window, cx);
                    }
                    ConversationEvent::QuestionAsked(question) => {
                        let body = format!("Question: {}", zp_tools::shorten(question, 120));
                        this.notify_background(conversation, body, window, cx);
                    }
                    ConversationEvent::Command(command) => {
                        use crate::conversation::composer::WindowCommand;
                        match command {
                            WindowCommand::NewChat => this.new_tab(window, cx),
                            WindowCommand::Settings => crate::settings::SettingsWindow::open(cx),
                            WindowCommand::Mcp => {
                                crate::settings::SettingsWindow::open_at(
                                    crate::settings::Page::Mcp,
                                    cx,
                                );
                            }
                            WindowCommand::Reload => this.reload_resources(cx),
                        }
                    }
                    ConversationEvent::OpenDiff(items) => {
                        this.open_diff(items.clone(), 0, window, cx)
                    }
                }
                cx.notify();
            },
        );
        Tab {
            conversation,
            pinned: false,
            preview,
            unread: false,
            _events: events,
        }
    }

    /// Opens `session` in a new tab (after the pinned ones) and activates it.
    pub(crate) fn add_tab(
        &mut self,
        session: Session,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let tab = self.make_tab(session, preview, window, cx);
        self.tabs.push(tab);
        let index = self.tabs.len() - 1;
        self.activate_tab(index, window, cx);
        index
    }

    /// A system notification about `conversation`, unless the user is looking at it.
    fn notify_background(
        &self,
        conversation: &Entity<ConversationView>,
        body: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        let visible = window.is_window_active()
            && self.tabs.get(self.active).map(|t| &t.conversation) == Some(conversation);
        if visible {
            return;
        }
        let title = format!("{} — {}", self.name, conversation.read(cx).title());
        crate::notifications::notify(window, conversation.entity_id(), title, body, cx);
    }

    /// Shows the tab of `conversation` (from a notification).
    pub(crate) fn activate_conversation(
        &mut self,
        conversation: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.conversation.entity_id() == conversation)
        {
            self.activate_tab(index, window, cx);
        }
    }

    /// A new empty chat.
    pub(crate) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = new_session(&self.root, cx);
        self.add_tab(session, false, window, cx);
    }

    pub(crate) fn activate_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.unread = false;
        self.active = index;
        let conversation = tab.conversation.clone();
        self.diff = None;
        self.changes.update(cx, |panel, cx| {
            panel.set_conversation(Some(conversation.clone()), cx)
        });
        self.sidebar.selected = conversation.read(cx).session_file();
        let focus = conversation.read(cx).composer_focus(cx);
        focus.focus(window, cx);
        self.save_tabs(cx);
        cx.notify();
    }

    pub(crate) fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len() as isize;
        if count == 0 {
            return;
        }
        let index = (self.active as isize + delta).rem_euclid(count) as usize;
        self.activate_tab(index, window, cx);
    }

    /// Opens session file `path`, reusing its tab if open. A preview replaces the
    /// current preview tab.
    pub(crate) fn open_session_file(
        &mut self,
        path: &Path,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.tab_for_file(path, cx) {
            if !preview {
                self.tabs[index].preview = false;
            }
            self.activate_tab(index, window, cx);
            return;
        }
        let session = match Session::open(path) {
            Ok(session) => session,
            Err(error) => {
                crate::workspace::report_error("Could not open session", &error, cx);
                return;
            }
        };
        let reusable = self
            .tabs
            .iter()
            .position(|tab| tab.preview && !tab.conversation.read(cx).is_running());
        match reusable.filter(|_| preview) {
            Some(index) => {
                self.tabs[index] = self.make_tab(session, true, window, cx);
                self.activate_tab(index, window, cx);
            }
            None => {
                self.add_tab(session, preview, window, cx);
            }
        }
    }

    /// Closes tab `index`, asking first if its agent is running.
    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let conversation = tab.conversation.clone();
        if !conversation.read(cx).is_running() {
            self.remove_tab(index, window, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "The agent in this tab is still running.",
            Some("Closing the tab stops it."),
            &["Stop and Close", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                conversation.update(cx, |view, cx| view.abort(cx));
                if let Some(index) = this.index_of(&conversation) {
                    this.remove_tab(index, window, cx);
                }
            });
        })
        .detach();
    }

    fn remove_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.new_tab(window, cx);
            return;
        }
        if self.active > index || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        let active = self.active;
        self.activate_tab(active, window, cx);
    }

    /// `secondary-w`: closes the active tab; closing the only, empty chat closes the
    /// window.
    pub(crate) fn close_active_tab(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let only_empty = self.tabs.len() == 1
            && self.tabs[0].conversation.read(cx).is_empty()
            && !self.tabs[0].conversation.read(cx).is_running();
        if only_empty || self.tabs.is_empty() {
            window.remove_window();
        } else {
            self.close_tab(self.active, window, cx);
        }
    }

    /// Closes every tab except `keep`, pinned and running ones.
    pub(crate) fn close_other_tabs(
        &mut self,
        keep: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keep_entity = self.tabs[keep].conversation.clone();
        let active_entity = self.tabs[self.active].conversation.clone();
        self.tabs.retain(|tab| {
            tab.conversation == keep_entity || tab.pinned || tab.conversation.read(cx).is_running()
        });
        let index = self
            .index_of(&active_entity)
            .or_else(|| self.index_of(&keep_entity))
            .unwrap_or(0);
        self.activate_tab(index, window, cx);
    }

    /// Moves a tab, keeping pinned tabs in front of the others.
    pub(crate) fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from >= self.tabs.len() || from == to {
            return;
        }
        let active = self.tabs[self.active].conversation.clone();
        let tab = self.tabs.remove(from);
        let to = to.min(self.tabs.len());
        self.tabs.insert(to, tab);
        self.tabs.sort_by_key(|tab| !tab.pinned);
        self.active = self.index_of(&active).unwrap_or(0);
        self.save_tabs(cx);
        cx.notify();
    }

    pub(crate) fn toggle_pin(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.pinned = !tab.pinned;
        tab.preview = false;
        let active = self.tabs[self.active].conversation.clone();
        self.tabs.sort_by_key(|tab| !tab.pinned);
        self.active = self.index_of(&active).unwrap_or(0);
        self.save_tabs(cx);
        cx.notify();
    }

    /// Persists the tabs of this project.
    pub(crate) fn save_tabs(&self, cx: &mut App) {
        let tabs: Vec<TabState> = self
            .tabs
            .iter()
            .map(|tab| {
                let view = tab.conversation.read(cx);
                TabState {
                    leaf_includes_restores: true,
                    session: view.session_file(),
                    leaf: view.leaf_id(),
                    pinned: tab.pinned,
                }
            })
            .collect();
        let root = self.root.clone();
        let active = self.active;
        AppStore::update(cx, move |state| {
            let project = state.project_mut(&root);
            project.tabs = tabs;
            project.active_tab = active;
        });
    }

    /// Reopens the tabs saved for this project (or one new chat).
    pub(crate) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = AppStore::state(cx).project(&self.root);
        let mut seen = std::collections::HashSet::new();
        for state in saved.tabs {
            let session = match &state.session {
                Some(path) if !seen.insert(path.clone()) => continue,
                Some(path) => match Session::open(path) {
                    Ok(session) => session,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "could not reopen tab");
                        continue;
                    }
                },
                // At most one unsaved chat is worth restoring.
                None if self
                    .tabs
                    .iter()
                    .any(|t| t.conversation.read(cx).session_file().is_none()) =>
                {
                    continue;
                }
                None => new_session(&self.root, cx),
            };
            let mut tab = self.make_tab(session, false, window, cx);
            tab.pinned = state.pinned;
            if let Some(leaf) = &state.leaf {
                tab.conversation.update(cx, |view, cx| {
                    view.restore_leaf(leaf, !state.leaf_includes_restores, cx)
                });
            }
            self.tabs.push(tab);
        }
        if self.tabs.is_empty() {
            let session = new_session(&self.root, cx);
            let tab = self.make_tab(session, false, window, cx);
            self.tabs.push(tab);
        }
        self.tabs.sort_by_key(|tab| !tab.pinned);
        let active = saved.active_tab.min(self.tabs.len() - 1);
        self.activate_tab(active, window, cx);
    }

    // ----- rendering ---------------------------------------------------------------

    pub(crate) fn tab_menu(
        &mut self,
        index: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let pinned = self.tabs[index].pinned;
        let entity_id = self.tabs[index].conversation.entity_id();
        let title = self.tabs[index].conversation.read(cx).title();
        let menu = ContextMenu::new(window, cx, |menu| {
            let pin = this.clone();
            let rename = this.clone();
            let duplicate = this.clone();
            let close = this.clone();
            let others = this.clone();
            menu.item(if pinned { "Unpin" } else { "Pin" }, move |_, cx| {
                let _ = pin.update(cx, |this, cx| {
                    if let Some(i) = this
                        .tabs
                        .iter()
                        .position(|t| t.conversation.entity_id() == entity_id)
                    {
                        this.toggle_pin(i, cx);
                    }
                });
            })
            .item("Rename…", move |window, cx| {
                let title = title.clone();
                let _ = rename.update(cx, |this, cx| {
                    this.start_rename(RenameTarget::Tab(entity_id), &title, position, window, cx)
                });
            })
            .item("Duplicate", move |window, cx| {
                let _ = duplicate.update(cx, |this, cx| this.duplicate_tab(entity_id, window, cx));
            })
            .separator()
            .item("Close", move |window, cx| {
                let _ = close.update(cx, |this, cx| {
                    if let Some(i) = this
                        .tabs
                        .iter()
                        .position(|t| t.conversation.entity_id() == entity_id)
                    {
                        this.close_tab(i, window, cx);
                    }
                });
            })
            .item("Close Others", move |window, cx| {
                let _ = others.update(cx, |this, cx| {
                    if let Some(i) = this
                        .tabs
                        .iter()
                        .position(|t| t.conversation.entity_id() == entity_id)
                    {
                        this.close_other_tabs(i, window, cx);
                    }
                });
            })
        });
        self.show_menu(menu, position, cx);
    }

    /// Opens a copy of a tab's current branch in a new tab.
    pub(crate) fn duplicate_tab(
        &mut self,
        entity_id: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .tabs
            .iter()
            .find(|t| t.conversation.entity_id() == entity_id)
        else {
            return;
        };
        match tab.conversation.read(cx).fork_current(cx) {
            Ok(session) => {
                self.add_tab(session, false, window, cx);
                self.load_sessions(cx);
            }
            Err(error) => crate::workspace::report_error("Could not duplicate", &error, cx),
        }
    }

    fn render_tab_status(&self, index: usize, cx: &App) -> Option<AnyElement> {
        let colors = &cx.theme().colors;
        let tab = &self.tabs[index];
        let view = tab.conversation.read(cx);
        let dot = |color| div().size(px(7.)).rounded_full().bg(color).flex_shrink_0();
        if view.needs_attention() {
            // Waiting for an approval: the most urgent state.
            Some(dot(colors.warning).into_any_element())
        } else if view.is_running() || view.is_compacting() {
            Some(
                dot(colors.accent)
                    .with_animation(
                        ("tab-running", index),
                        Animation::new(Duration::from_millis(1200)).repeat(),
                        |dot, t| dot.opacity(0.35 + 0.65 * (t * std::f32::consts::PI).sin()),
                    )
                    .into_any_element(),
            )
        } else if view.has_failed() {
            Some(dot(colors.error).into_any_element())
        } else if tab.unread {
            Some(dot(colors.accent).into_any_element())
        } else {
            None
        }
    }

    pub(crate) fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let tabs = (0..self.tabs.len()).map(|index| {
            let tab = &self.tabs[index];
            let active = index == self.active;
            let title: SharedString = tab.conversation.read(cx).title().into();
            let group = SharedString::from(format!("tab-{index}"));
            let side_width = px(if tab.pinned { 48. } else { 16. });
            div()
                .id(("tab", index))
                .group(group.clone())
                .h_full()
                .flex()
                .items_center()
                .gap_1p5()
                .flex_shrink_0()
                .min_w(px(80.))
                .max_w(px(220.))
                .px_1p5()
                .border_r_1()
                .border_color(colors.border)
                .cursor_pointer()
                .when(active, |this| this.bg(colors.background))
                .when(!active, |this| {
                    this.border_b_1()
                        .text_color(colors.text_muted)
                        .hover_style(|s| s.bg(colors.surface_hover))
                })
                .child(
                    div()
                        .w(side_width)
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .when(tab.pinned, |this| {
                            this.child(
                                div()
                                    .px_1()
                                    .rounded(px(3.))
                                    .text_size(px(10.))
                                    .text_color(colors.text_muted)
                                    .bg(colors.code_background)
                                    .child("pinned"),
                            )
                        })
                        .children(self.render_tab_status(index, cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_center()
                        .truncate()
                        .when(tab.preview, |this| this.italic())
                        .child(title.clone()),
                )
                .when(!tab.pinned, |this| {
                    this.child(
                        div()
                            .id(("close-tab", index))
                            .flex_shrink_0()
                            .w(px(16.))
                            .text_center()
                            .rounded(px(3.))
                            .text_color(colors.text_muted)
                            .invisible()
                            .group_hover_style(group, |s| s.visible())
                            .hover_style(|s| s.bg(colors.surface_hover).text_color(colors.text))
                            .child("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(index, window, cx)
                            })),
                    )
                })
                .when(tab.pinned, |this| {
                    this.child(div().w(side_width).flex_shrink_0())
                })
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() >= 2
                            && let Some(tab) = this.tabs.get_mut(index)
                        {
                            tab.preview = false;
                        }
                        this.activate_tab(index, window, cx)
                    }),
                )
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| this.close_tab(index, window, cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        this.tab_menu(index, event.position, window, cx)
                    }),
                )
                .on_drag(
                    DraggedTab {
                        index,
                        title: title.clone(),
                    },
                    |dragged, _, _, cx| cx.new(|_| TabGhost(dragged.title.clone())),
                )
                .on_drag_over::<DraggedTab>(move |style, _, _, _| {
                    style.border_l_2().border_color(gpui::blue())
                })
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                    this.move_tab(dragged.index, index, cx)
                }))
        });
        let tree_open = self
            .active_conversation()
            .is_some_and(|c| c.read(cx).is_tree_open());
        div()
            .id("tab-bar")
            .relative()
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(colors.surface)
            // The active tab paints over this line to join the conversation below.
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .w_full()
                    .h(px(1.))
                    .bg(colors.border),
            )
            .child(
                div()
                    .id("tabs")
                    .h_full()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .child(
                div()
                    .id("new-tab")
                    .w(px(32.))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(colors.text_muted)
                    .cursor_pointer()
                    .hover_style(|s| s.text_color(colors.text))
                    .child("+")
                    .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx)))
                    .on_drop(cx.listener(|this, dragged: &DraggedTab, _, cx| {
                        let last = this.tabs.len().saturating_sub(1);
                        this.move_tab(dragged.index, last, cx)
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("tree-toggle")
                    .mb_1()
                    .mr_2()
                    .px_2()
                    .py_0p5()
                    .rounded(px(4.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .text_color(if tree_open {
                        colors.text
                    } else {
                        colors.text_muted
                    })
                    .when(tree_open, |this| this.bg(colors.surface_hover))
                    .hover_style(|s| s.text_color(colors.text))
                    .child("Tree")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_session_tree(&crate::actions::ToggleSessionTree, window, cx)
                    })),
            )
    }
}
