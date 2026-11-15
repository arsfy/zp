//! The sessions sidebar: date groups, search, preview / open, and a context menu.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, TimeZone as _};
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable as _, FontWeight, IntoElement,
    MouseButton, MouseDownEvent, Pixels, Point, Window, div, prelude::*, px, rgb_to_hsla,
};
use gpui_elements::editable_text::{EditableTextState, text_input};
use zp_session::{Session, SessionInfo};

use super::{ProjectWindow, RenameTarget, session_manager};
use crate::context_menu::ContextMenu;
use crate::store::{AppPaths, FileTrash};
use crate::theme::ActiveTheme;
use crate::ui::{CUSTOM_TITLE_BAR, relative_time};

/// The date group of a session in the sidebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionGroup {
    Today,
    Yesterday,
    LastWeek,
    Older,
}

impl SessionGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "TODAY",
            Self::Yesterday => "YESTERDAY",
            Self::LastWeek => "LAST 7 DAYS",
            Self::Older => "OLDER",
        }
    }
}

/// The group of a session last active at `modified_ms`, in local time relative to `now`.
pub fn group_for(modified_ms: i64, now: DateTime<Local>) -> SessionGroup {
    let Some(modified) = Local.timestamp_millis_opt(modified_ms).single() else {
        return SessionGroup::Older;
    };
    let days = now
        .date_naive()
        .signed_duration_since(modified.date_naive())
        .num_days();
    match days {
        i64::MIN..=0 => SessionGroup::Today,
        1 => SessionGroup::Yesterday,
        2..=6 => SessionGroup::LastWeek,
        _ => SessionGroup::Older,
    }
}

/// Sessions shown in the sidebar.
pub(super) enum SessionList {
    Loading,
    Loaded(Vec<SessionInfo>),
    Unavailable(String),
}

pub(super) struct Sidebar {
    pub sessions: SessionList,
    pub search: Entity<EditableTextState>,
    /// The highlighted session (keyboard navigation, last opened).
    pub selected: Option<PathBuf>,
}

impl Sidebar {
    pub fn new(search: Entity<EditableTextState>) -> Self {
        Self {
            sessions: SessionList::Loading,
            search,
            selected: None,
        }
    }

    pub fn on_search_changed(&mut self) {}
}

impl ProjectWindow {
    pub(crate) fn load_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = AppPaths::sessions_dir(cx) else {
            self.sidebar.sessions = SessionList::Unavailable("No data directory".into());
            return;
        };
        let root = self.root.clone();
        let listing = cx
            .background_executor()
            .spawn(async move { zp_session::SessionManager::new(dir).list(&root) });
        cx.spawn(async move |this, cx| {
            let result = listing.await;
            let _ = this.update(cx, |this, cx| {
                this.sidebar.sessions = match result {
                    Ok(sessions) => SessionList::Loaded(sessions),
                    Err(error) => SessionList::Unavailable(format!("{error:#}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Listed sessions matching the search, most recent first.
    pub(crate) fn visible_sessions(&self, cx: &App) -> Vec<SessionInfo> {
        let SessionList::Loaded(sessions) = &self.sidebar.sessions else {
            return Vec::new();
        };
        let query = self.sidebar.search.read(cx).as_str().trim().to_owned();
        sessions
            .iter()
            .filter(|info| info.matches(&query))
            .cloned()
            .collect()
    }

    pub(crate) fn on_session_click(
        &mut self,
        path: PathBuf,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.selected = Some(path.clone());
        // A single click previews; a double click (or Enter) opens for good.
        let permanent = event.click_count() >= 2;
        self.open_session_file(&path, !permanent, window, cx);
    }

    pub(crate) fn move_session_selection(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sessions = self.visible_sessions(cx);
        if sessions.is_empty() {
            return;
        }
        let current = self
            .sidebar
            .selected
            .as_ref()
            .and_then(|selected| sessions.iter().position(|info| &info.path == selected));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, sessions.len() as isize - 1) as usize,
            None if delta > 0 => 0,
            None => sessions.len() - 1,
        };
        let path = sessions[next].path.clone();
        self.sidebar.selected = Some(path.clone());
        self.open_session_file(&path, true, window, cx);
        // Keep typing in the search field.
        self.sidebar
            .search
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    pub(crate) fn open_selected_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.sidebar.selected.clone().or_else(|| {
            self.visible_sessions(cx)
                .first()
                .map(|info| info.path.clone())
        });
        if let Some(path) = selected {
            self.open_session_file(&path, false, window, cx);
        }
    }

    /// Sets the search text (tests).
    #[cfg(test)]
    pub fn set_session_search(&mut self, query: &str, cx: &mut Context<Self>) {
        self.sidebar
            .search
            .update(cx, |search, cx| search.emplace(query, cx));
    }

    pub(crate) fn clear_session_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar
            .search
            .update(cx, |search, cx| search.emplace("", cx));
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(crate) fn session_menu(
        &mut self,
        info: &SessionInfo,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let path = info.path.clone();
        let title = info.title().to_owned();
        let menu = ContextMenu::new(window, cx, |menu| {
            let (open, rename, duplicate, delete) =
                (this.clone(), this.clone(), this.clone(), this.clone());
            let (open_path, rename_path, duplicate_path, delete_path, reveal_path) = (
                path.clone(),
                path.clone(),
                path.clone(),
                path.clone(),
                path.clone(),
            );
            menu.item("Open in New Tab", move |window, cx| {
                let _ = open.update(cx, |this, cx| {
                    this.open_session_file(&open_path, false, window, cx)
                });
            })
            .item("Rename…", move |window, cx| {
                let title = title.clone();
                let path = rename_path.clone();
                let _ = rename.update(cx, |this, cx| {
                    this.start_rename(RenameTarget::Session(path), &title, position, window, cx)
                });
            })
            .item("Duplicate", move |window, cx| {
                let _ = duplicate.update(cx, |this, cx| {
                    this.duplicate_session(&duplicate_path, window, cx)
                });
            })
            .item(reveal_label(), move |_, cx| cx.reveal_path(&reveal_path))
            .separator()
            .destructive_item("Delete", move |window, cx| {
                let _ = delete.update(cx, |this, cx| this.delete_session(&delete_path, window, cx));
            })
        });
        self.show_menu(menu, position, cx);
    }

    /// Forks a session into a new file and opens it.
    pub(crate) fn duplicate_session(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.tab_for_file(path, cx) {
            let id = self.tabs[index].conversation.entity_id();
            self.duplicate_tab(id, window, cx);
            return;
        }
        let result = (|| -> anyhow::Result<Session> {
            let session = Session::open(path)?;
            let leaf = session
                .leaf_id()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("the session is empty"))?;
            match session_manager(cx) {
                Some(manager) => manager.fork(&session, &leaf),
                None => session.fork(&leaf, None),
            }
        })();
        match result {
            Ok(session) => {
                self.add_tab(session, false, window, cx);
                self.load_sessions(cx);
            }
            Err(error) => crate::workspace::report_error("Could not duplicate", &error, cx),
        }
    }

    /// Moves a session file to the trash (closing its tab).
    pub(crate) fn delete_session(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.tab_for_file(path, cx) {
            if self.tabs[index].conversation.read(cx).is_running() {
                crate::workspace::report_error(
                    "Could not delete",
                    &anyhow::anyhow!("the session is running; stop it first"),
                    cx,
                );
                return;
            }
            self.close_tab(index, window, cx);
        }
        let trash = cx
            .try_global::<FileTrash>()
            .map_or(FileTrash::system().0, |t| t.0);
        if let Err(error) = trash(path) {
            crate::workspace::report_error("Could not delete", &error, cx);
        }
        if self.sidebar.selected.as_deref() == Some(path) {
            self.sidebar.selected = None;
        }
        self.load_sessions(cx);
        cx.notify();
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let radius = cx.theme().metrics.radius;
        let now_ms = zp_ai::now_ms();
        let now = Local::now();
        let highlighted = self.sidebar.selected.clone().or_else(|| {
            self.active_conversation()
                .and_then(|c| c.read(cx).session_file())
        });
        let open: Vec<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.conversation.read(cx).session_file())
            .collect();
        let searching = !self.sidebar.search.read(cx).as_str().trim().is_empty();

        let body: AnyElement = match &self.sidebar.sessions {
            SessionList::Loading => muted("Loading…", cx),
            SessionList::Unavailable(reason) => muted(reason, cx),
            SessionList::Loaded(all) if all.is_empty() => muted("No sessions yet", cx),
            SessionList::Loaded(_) => {
                let sessions = self.visible_sessions(cx);
                if sessions.is_empty() {
                    muted("No matching sessions", cx)
                } else {
                    let mut list = div()
                        .id("sessions")
                        .size_full()
                        .flex()
                        .flex_col()
                        .px_1p5()
                        .pb_2()
                        .overflow_y_scroll();
                    let mut last_group = None;
                    for (index, info) in sessions.into_iter().enumerate() {
                        let group = group_for(info.modified, now);
                        if last_group != Some(group) {
                            last_group = Some(group);
                            list = list.child(
                                div()
                                    .px_1p5()
                                    .pt_2()
                                    .pb_0p5()
                                    .text_size(px(10.))
                                    .text_color(colors.text_muted)
                                    .child(group.label()),
                            );
                        }
                        let is_highlighted = highlighted.as_deref() == Some(info.path.as_path());
                        let is_open = open.contains(&info.path);
                        let path = info.path.clone();
                        let menu_info = info.clone();
                        list = list.child(
                            div()
                                .id(("session", index))
                                .flex()
                                .flex_col()
                                .px_1p5()
                                .py_1()
                                .rounded(radius)
                                .cursor_pointer()
                                .when(is_highlighted, |this| this.bg(colors.surface_hover))
                                .hover_style(|style| style.bg(colors.surface_hover))
                                .on_click(cx.listener(move |this, event, window, cx| {
                                    this.on_session_click(path.clone(), event, window, cx)
                                }))
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        this.session_menu(&menu_info, event.position, window, cx)
                                    }),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(info.title().to_owned()),
                                        )
                                        .when(is_open, |this| {
                                            this.child(
                                                div()
                                                    .text_size(px(9.))
                                                    .text_color(colors.accent)
                                                    .child("●"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(colors.text_muted)
                                        .child(format!(
                                            "{} · {} messages",
                                            relative_time(info.modified, now_ms),
                                            info.message_count
                                        )),
                                ),
                        );
                    }
                    list.into_any_element()
                }
            }
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_size(px(11.))
                    .text_color(colors.text_muted)
                    .child("SESSIONS")
                    .child(
                        div()
                            .id("sidebar-new-chat")
                            .px_1()
                            .rounded(px(3.))
                            .cursor_pointer()
                            .hover_style(|s| s.text_color(colors.text).bg(colors.surface_hover))
                            .child("+")
                            .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
                    ),
            )
            .child(
                div().px_3().pb_1().key_context("SessionSearch").child(
                    text_input("session-search")
                        .state(self.sidebar.search.downgrade())
                        .placeholder("Search sessions…")
                        .placeholder_color(rgb_to_hsla(colors.text_muted))
                        .caret_color(rgb_to_hsla(colors.text))
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(if searching {
                            colors.accent
                        } else {
                            colors.border
                        })
                        .bg(colors.background)
                        .text_size(px(12.))
                        .whitespace_nowrap(),
                ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

fn muted(text: &str, cx: &App) -> AnyElement {
    div()
        .px_3()
        .pt_1()
        .text_color(cx.theme().colors.text_muted)
        .child(text.to_owned())
        .into_any_element()
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(target_os = "windows") {
        "Reveal in File Explorer"
    } else {
        "Reveal in File Manager"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn groups_by_local_day() {
        let now = Local::now();
        let at = |delta: Duration| (now - delta).timestamp_millis();
        assert_eq!(group_for(at(Duration::zero()), now), SessionGroup::Today);
        // Midnight boundaries depend on the time of day, so check whole-day offsets.
        let noon = now
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .and_then(|t| Local.from_local_datetime(&t).single())
            .unwrap();
        let day = |days: i64| (noon - Duration::days(days)).timestamp_millis();
        assert_eq!(group_for(day(0), noon), SessionGroup::Today);
        assert_eq!(group_for(day(1), noon), SessionGroup::Yesterday);
        assert_eq!(group_for(day(2), noon), SessionGroup::LastWeek);
        assert_eq!(group_for(day(6), noon), SessionGroup::LastWeek);
        assert_eq!(group_for(day(7), noon), SessionGroup::Older);
        assert_eq!(group_for(day(400), noon), SessionGroup::Older);
        // Clock skew: future timestamps count as today.
        assert_eq!(group_for(at(-Duration::hours(1)), now), SessionGroup::Today);
    }
}
