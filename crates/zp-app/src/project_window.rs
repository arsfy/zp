//! The window of one project: sidebar, tabs, conversation area, changes panel and
//! status bar. The conversation itself arrives in M5; this milestone provides the frame.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, ColorExt as _, Context, DragMoveEvent, FocusHandle, Focusable, FontWeight,
    IntoElement, Render, Subscription, Window, div, prelude::*, px,
};
use zp_core::app_state::PanelLayout;
use zp_session::{SessionInfo, SessionManager};

use crate::actions::{PROJECT_CONTEXT, ToggleRightPanel, ToggleSidebar};
use crate::store::{AppModels, AppPaths, AppStore};
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{CUSTOM_TITLE_BAR, relative_time, section_header, title_bar};
use crate::workspace::{self, Workspaces};

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

/// Sessions shown in the sidebar.
enum SessionList {
    Loading,
    Loaded(Vec<SessionInfo>),
    Unavailable(String),
}

/// Root view of a project window.
pub struct ProjectWindow {
    root: PathBuf,
    name: String,
    branch: Option<String>,
    layout: PanelLayout,
    sessions: SessionList,
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

    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let layout = AppStore::state(cx).project(&root).layout;

        let geometry_root = root.clone();
        let release_root = root.clone();
        let subscriptions = vec![
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
            cx.on_release(move |_, cx| Workspaces::project_closed(&release_root, cx)),
        ];

        let mut this = Self {
            name: zp_core::project::project_name(&root),
            branch: None,
            root,
            layout,
            sessions: SessionList::Loading,
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
        cx.notify();
    }

    fn load_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = AppPaths::sessions_dir(cx) else {
            self.sessions = SessionList::Unavailable("No data directory".into());
            return;
        };
        let root = self.root.clone();
        let listing = cx
            .background_executor()
            .spawn(async move { SessionManager::new(dir).list(&root) });
        cx.spawn(async move |this, cx| {
            let result = listing.await;
            let _ = this.update(cx, |this, cx| {
                this.sessions = match result {
                    Ok(sessions) => SessionList::Loaded(sessions),
                    Err(error) => SessionList::Unavailable(format!("{error:#}")),
                };
                cx.notify();
            });
        })
        .detach();
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

    fn render_sidebar(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let radius = cx.theme().metrics.radius;
        let now = zp_ai::now_ms();
        let body: AnyElement = match &self.sessions {
            SessionList::Loading => div()
                .px_3()
                .text_color(colors.text_muted)
                .child("Loading…")
                .into_any_element(),
            SessionList::Unavailable(reason) => div()
                .px_3()
                .text_color(colors.text_muted)
                .child(reason.clone())
                .into_any_element(),
            SessionList::Loaded(sessions) if sessions.is_empty() => div()
                .px_3()
                .text_color(colors.text_muted)
                .child("No sessions yet")
                .into_any_element(),
            SessionList::Loaded(sessions) => div()
                .id("sessions")
                .flex()
                .flex_col()
                .px_1p5()
                .overflow_y_scroll()
                .children(sessions.iter().enumerate().map(|(index, info)| {
                    div()
                        .id(("session", index))
                        .flex()
                        .flex_col()
                        .px_1p5()
                        .py_1()
                        .rounded(radius)
                        .hover_style(|style| style.bg(colors.surface_hover))
                        .child(div().truncate().child(info.title().to_owned()))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(colors.text_muted)
                                .child(format!(
                                    "{} · {} messages",
                                    relative_time(info.modified, now),
                                    info.message_count
                                )),
                        )
                }))
                .into_any_element(),
        };
        div()
            .id("sidebar")
            .w(px(self.layout.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(colors.surface)
            .border_r_1()
            .border_color(colors.border)
            // The custom title row already shows the project name.
            .when(!CUSTOM_TITLE_BAR, |this| {
                this.child(
                    div()
                        .px_3()
                        .pt_3()
                        .font_weight(FontWeight::SEMIBOLD)
                        .truncate()
                        .child(self.name.clone()),
                )
            })
            .child(section_header("SESSIONS", cx))
            .child(div().flex_1().min_h_0().child(body))
    }

    fn render_tab_bar(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .id("tab-bar")
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_end()
            .px_2()
            .gap_1()
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("tab-0")
                    .px_3()
                    .py_1p5()
                    .rounded_t(cx.theme().metrics.radius)
                    .bg(colors.background)
                    .border_1()
                    .border_b_0()
                    .border_color(colors.border)
                    .child("New Chat"),
            )
            .child(
                div()
                    .id("new-tab")
                    .px_2()
                    .py_1p5()
                    .text_color(colors.text_muted)
                    .child("+"),
            )
    }

    fn render_center(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let radius = cx.theme().metrics.radius;
        div()
            .id("center")
            .flex_1()
            .min_w(px(CENTER_MIN_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_tab_bar(cx))
            .child(
                div()
                    .id("conversation")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.name.clone()),
                    )
                    .child(
                        div()
                            .text_color(colors.text_muted)
                            .child(zp_core::project::tilde_path(&self.root)),
                    ),
            )
            .child(
                div().flex_shrink_0().p_3().child(
                    div()
                        .id("composer")
                        .min_h(px(72.))
                        .px_3()
                        .py_2()
                        .rounded(radius)
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.surface)
                        .text_color(colors.text_muted)
                        .child("Message ZP…"),
                ),
            )
    }

    fn render_right_panel(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .id("right-panel")
            .w(px(self.layout.right_panel_width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(colors.surface)
            .border_l_1()
            .border_color(colors.border)
            .child(section_header("CHANGES", cx))
            .child(
                div()
                    .px_3()
                    .text_color(colors.text_muted)
                    .child("No changes yet"),
            )
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

    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let model = AppModels::default_model(cx);
        let model_label = model
            .as_ref()
            .map_or_else(|| "No model configured".to_owned(), |m| m.qualified_id());
        let mut right = vec![model_label];
        if model.is_some() {
            right.extend([
                "thinking off".to_owned(),
                "0 tokens".to_owned(),
                "$0.00".to_owned(),
                "context 0%".to_owned(),
            ]);
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
                div().child(
                    self.branch
                        .as_ref()
                        .map(|branch| format!("⎇ {branch}"))
                        .unwrap_or_default(),
                ),
            )
            .child(div().child(right.join("  ·  ")))
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
        div()
            .id("project-window")
            .key_context(PROJECT_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_right_panel))
            .on_drag_move(cx.listener(Self::on_resize_drag))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .text_size(theme.metrics.font_size)
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
