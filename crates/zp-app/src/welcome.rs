//! The welcome window: open a folder, start a new window, or pick a recent project.

use std::path::PathBuf;

use gpui::{
    Context, FocusHandle, Focusable, FontWeight, IntoElement, Render, Subscription, Window, div,
    prelude::*, px,
};

use crate::actions::{NewWindow, OpenFolder, shortcut_keys};
use crate::store::AppStore;
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{CUSTOM_TITLE_BAR, button, section_header, title_bar};
use crate::workspace::{self, Workspaces};

/// Root view of a welcome window.
pub struct WelcomeView {
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl WelcomeView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let handle = window.window_handle();
        let subscriptions = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                if Theme::sync_with_appearance(window.appearance(), cx) {
                    cx.refresh_windows();
                }
            }),
            cx.observe_window_bounds(window, |_, window, cx| {
                let geometry = workspace::capture_geometry(window, cx);
                AppStore::update(cx, |state| state.welcome_window = Some(geometry));
            }),
            cx.observe_global::<AppStore>(|_, cx| cx.notify()),
            cx.on_release(move |_, cx| Workspaces::welcome_closed(handle, cx)),
        ];
        Self {
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        workspace::prompt_open_folder(Some(window.window_handle()), cx);
    }

    fn open_recent(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let handle = window.window_handle();
        if let Err(error) = workspace::open_project(&path, Some(handle), cx) {
            AppStore::update(cx, |state| state.remove_recent(&path));
            workspace::report_error("Could not open project", &error, cx);
        }
    }
}

impl Focusable for WelcomeView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WelcomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let radius = theme.metrics.radius;
        let recent = AppStore::state(cx).recent_projects.clone();

        let recent_list = div()
            .id("recent-projects")
            .flex()
            .flex_col()
            .max_h(px(280.))
            .overflow_y_scroll()
            .children(recent.into_iter().enumerate().map(|(index, project)| {
                let exists = project.path.is_dir();
                let name = zp_core::project::project_name(&project.path);
                let location = zp_core::project::tilde_path(&project.path);
                let path = project.path.clone();
                div()
                    .id(("recent", index))
                    .flex()
                    .flex_col()
                    .px_3()
                    .py_1p5()
                    .rounded(radius)
                    .cursor_pointer()
                    .hover_style(|style| style.bg(colors.surface_hover))
                    .when(!exists, |this| this.opacity(0.5))
                    .child(div().font_weight(FontWeight::MEDIUM).child(name))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .truncate()
                            .child(if exists {
                                location
                            } else {
                                format!("{location} (missing)")
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_recent(path.clone(), window, cx)
                    }))
            }));
        let has_recent = !AppStore::state(cx).recent_projects.is_empty();

        div()
            .id("welcome")
            .key_context("Welcome")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::open_folder))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .text_size(theme.metrics.font_size)
            .when(CUSTOM_TITLE_BAR, |this| this.child(title_bar("title-bar")))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .child(self.render_content(recent_list.into_any_element(), has_recent, cx)),
            )
    }
}

impl WelcomeView {
    fn render_content(
        &self,
        recent_list: gpui::AnyElement,
        has_recent: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .w(px(460.))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(40.))
                    .font_weight(FontWeight::BOLD)
                    .child(zp_core::APP_NAME),
            )
            .child(
                div()
                    .text_color(colors.text_muted)
                    .child("A native GUI agent harness"),
            )
            .child(
                div()
                    .mt_6()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(button(
                        "open-folder",
                        "Open Folder…",
                        shortcut_keys("zp::OpenFolder", cx).as_deref(),
                        cx,
                        |_, window, cx| window.dispatch_action(Box::new(OpenFolder), cx),
                    ))
                    .child(button(
                        "new-window",
                        "New Window",
                        shortcut_keys("zp::NewWindow", cx).as_deref(),
                        cx,
                        |_, window, cx| window.dispatch_action(Box::new(NewWindow), cx),
                    )),
            )
            .child(
                div()
                    .mt_4()
                    .child(section_header("RECENT PROJECTS", cx).px_0()),
            )
            .child(if has_recent {
                recent_list
            } else {
                div()
                    .text_color(colors.text_muted)
                    .child("No recent projects")
                    .into_any_element()
            })
    }
}
