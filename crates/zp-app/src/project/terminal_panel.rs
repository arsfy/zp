//! The terminal panel under the conversation, toggled from the status bar (or ctrl-`).
//! Like VS Code's, it holds several terminals: the active one fills the panel and, with
//! more than one, a list on the right switches between them.

use gpui::{
    App, Context, DragMoveEvent, Entity, EntityId, Focusable as _, FontWeight, IntoElement,
    Subscription, Window, div, prelude::*, px, svg,
};

use super::{NoPreview, ProjectWindow, RenameTarget};
use crate::context_menu::ContextMenu;
use crate::terminal::{Shell, TerminalEvent, TerminalView};
use crate::theme::ActiveTheme;

const ICON_TERMINAL: &[u8] = include_bytes!("../../../../assets/icons/square-terminal.svg");
const ICON_NEW: &[u8] = include_bytes!("../../../../assets/icons/plus.svg");
const ICON_KILL: &[u8] = include_bytes!("../../../../assets/icons/trash-2.svg");
const ICON_CLOSE: &[u8] = include_bytes!("../../../../assets/icons/x.svg");
const ICON_MAXIMIZE: &[u8] = include_bytes!("../../../../assets/icons/chevron-up.svg");
const ICON_RESTORE: &[u8] = include_bytes!("../../../../assets/icons/chevron-down.svg");
/// Smallest terminal panel, and the least room left above it.
const MIN_HEIGHT: f32 = 100.0;
const MIN_ABOVE: f32 = 160.0;
const HEADER_HEIGHT: f32 = 30.0;
/// Limits of the terminal list's width.
const LIST_WIDTH: (f32, f32) = (110.0, 420.0);

/// Drag payload of the terminal panel's top edge.
#[derive(Clone, Copy, Debug)]
pub(super) struct TerminalResize;

/// Drag payload of the terminal list's left edge.
#[derive(Clone, Copy, Debug)]
pub(super) struct TerminalListResize;

struct TerminalTab {
    view: Entity<TerminalView>,
    /// A name the user gave it (instead of the program's).
    name: Option<String>,
    _events: Subscription,
}

/// The panel's terminals.
#[derive(Default)]
pub(super) struct Terminals {
    tabs: Vec<TerminalTab>,
    active: usize,
    /// Why the last shell could not start.
    error: Option<String>,
}

impl Terminals {
    fn active(&self) -> Option<&Entity<TerminalView>> {
        self.tabs.get(self.active).map(|tab| &tab.view)
    }
}

/// The user's shell, by name (`zsh`), for tab labels.
fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|shell| {
            std::path::Path::new(&shell)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "powershell" } else { "shell" }.to_owned())
}

impl ProjectWindow {
    /// The active terminal, if any (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn terminal_for_screenshot(&self) -> Option<Entity<TerminalView>> {
        self.terminals.active().cloned()
    }

    /// Ids of the terminals (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn terminal_tabs_for_screenshot(&self) -> Vec<EntityId> {
        self.terminals
            .tabs
            .iter()
            .map(|tab| tab.view.entity_id())
            .collect()
    }

    /// The active terminal, if any (tests).
    #[cfg(test)]
    pub fn terminal(&self) -> Option<Entity<TerminalView>> {
        self.terminals.active().cloned()
    }

    /// Every terminal, in list order, and the active index (tests).
    #[cfg(test)]
    pub fn terminal_tabs(&self) -> (Vec<Entity<TerminalView>>, usize) {
        (
            self.terminals
                .tabs
                .iter()
                .map(|tab| tab.view.clone())
                .collect(),
            self.terminals.active,
        )
    }

    /// Shows the panel (starting a shell if there is none) and focuses it, or hides it.
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.terminal_visible {
            self.layout.terminal_visible = false;
            self.focus_active(window, cx);
        } else {
            self.layout.terminal_visible = true;
            self.ensure_terminal(window, cx);
            self.focus_terminal(window, cx);
        }
        self.save_layout(cx);
        cx.notify();
    }

    /// Starts a terminal unless one is running.
    pub(super) fn ensure_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.tabs.is_empty() {
            self.spawn_terminal(window, cx);
        }
    }

    /// Opens another terminal and shows it.
    pub fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.spawn_terminal(window, cx);
        if !self.layout.terminal_visible {
            self.layout.terminal_visible = true;
            self.save_layout(cx);
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }

    fn spawn_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match Shell::spawn(&self.root) {
            Ok(shell) => {
                let view = cx.new(|cx| TerminalView::new(shell, cx));
                let events = cx.subscribe_in(
                    &view,
                    window,
                    |this, view, event: &TerminalEvent, window, cx| match event {
                        // Like VS Code, a terminal whose shell exits goes away.
                        TerminalEvent::Exited => this.close_terminal(view.entity_id(), window, cx),
                        TerminalEvent::TitleChanged => cx.notify(),
                    },
                );
                self.terminals.tabs.push(TerminalTab {
                    view,
                    name: None,
                    _events: events,
                });
                self.terminals.active = self.terminals.tabs.len() - 1;
                self.terminals.error = None;
            }
            Err(error) => {
                self.terminals.error = Some(format!("Could not start a shell: {error:#}"));
            }
        }
    }

    pub(super) fn focus_terminal(&self, window: &mut Window, cx: &mut App) {
        if let Some(view) = self.terminals.active() {
            view.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// Shows terminal `index`.
    fn activate_terminal(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.terminals.tabs.len() {
            self.terminals.active = index;
            self.focus_terminal(window, cx);
            cx.notify();
        }
    }

    /// Ends terminal `id`; the panel hides once none is left.
    pub fn close_terminal(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .terminals
            .tabs
            .iter()
            .position(|tab| tab.view.entity_id() == id)
        else {
            return;
        };
        let had_focus = self.terminals.tabs[index]
            .view
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        self.terminals.tabs.remove(index);
        if self.terminals.active > index || self.terminals.active >= self.terminals.tabs.len() {
            self.terminals.active = self.terminals.active.saturating_sub(1);
        }
        if self.terminals.tabs.is_empty() {
            self.layout.terminal_visible = false;
            self.save_layout(cx);
            self.focus_active(window, cx);
        } else if had_focus {
            self.focus_terminal(window, cx);
        }
        cx.notify();
    }

    /// Whether the panel takes the whole height of the conversation column.
    pub(super) fn terminal_maximized(&self) -> bool {
        self.layout.terminal_visible && self.layout.terminal_maximized
    }

    /// Lets the panel take the whole height, or gives it back.
    pub fn toggle_terminal_maximized(&mut self, cx: &mut Context<Self>) {
        self.layout.terminal_maximized = !self.layout.terminal_maximized;
        self.save_layout(cx);
        cx.notify();
    }

    /// Names terminal `id` (`None`: follow the program again).
    pub fn rename_terminal(
        &mut self,
        id: EntityId,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self
            .terminals
            .tabs
            .iter_mut()
            .find(|tab| tab.view.entity_id() == id)
        {
            tab.name = name;
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }

    fn start_terminal_rename(
        &mut self,
        id: EntityId,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .terminals
            .tabs
            .iter()
            .find(|tab| tab.view.entity_id() == id)
        else {
            return;
        };
        let current = Self::terminal_label(tab, cx);
        self.start_rename(RenameTarget::Terminal(id), &current, position, window, cx);
    }

    /// The menu of terminal `id`: rename, kill.
    fn terminal_menu(
        &mut self,
        id: EntityId,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::new(window, cx, |menu| {
            let (rename, kill) = (this.clone(), this);
            menu.item("Rename…", move |window, cx| {
                let _ = rename.update(cx, |this, cx| {
                    this.start_terminal_rename(id, position, window, cx)
                });
            })
            .separator()
            .item("Kill Terminal", move |window, cx| {
                let _ = kill.update(cx, |this, cx| this.close_terminal(id, window, cx));
            })
        });
        self.show_menu(menu, position, cx);
    }

    pub(super) fn on_terminal_list_resize(
        &mut self,
        event: &DragMoveEvent<TerminalListResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = f32::from(event.bounds.right() - event.event.position.x);
        let max = (f32::from(event.bounds.size.width) / 2.0).max(LIST_WIDTH.0);
        self.layout.terminal_list_width = width.clamp(LIST_WIDTH.0, LIST_WIDTH.1.min(max));
        self.save_layout(cx);
        cx.notify();
    }

    pub(super) fn on_terminal_resize(
        &mut self,
        event: &DragMoveEvent<TerminalResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        let height = f32::from(bounds.bottom() - event.event.position.y);
        let max = (f32::from(bounds.size.height) - MIN_ABOVE).max(MIN_HEIGHT);
        self.layout.terminal_height = height.clamp(MIN_HEIGHT, max);
        self.save_layout(cx);
        cx.notify();
    }

    /// The status bar's terminal button.
    pub(super) fn render_terminal_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let open = self.layout.terminal_visible;
        div()
            .id("toggle-terminal")
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor_pointer()
            .when(open, |this| {
                this.bg(gpui::ColorExt::opacity(
                    &gpui::rgb_to_hsla(colors.accent),
                    0.18,
                ))
            })
            .hover_style(move |s| s.bg(colors.surface_hover))
            .tooltip(crate::ui::tooltip("Toggle Terminal (ctrl-`)"))
            .child(svg().data(ICON_TERMINAL).size(px(15.)).text_color(if open {
                colors.accent
            } else {
                colors.text_muted
            }))
            .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx)))
    }

    fn panel_icon_button(
        &self,
        id: impl Into<gpui::ElementId>,
        icon: &'static [u8],
        tooltip: &'static str,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors.clone();
        div()
            .id(id)
            .size(px(22.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .cursor_pointer()
            .hover_style(move |s| s.bg(colors.surface_hover))
            .tooltip(crate::ui::tooltip(tooltip))
            .child(svg().data(icon).size(px(14.)).text_color(colors.text_muted))
    }

    /// The label of a terminal: the name the user gave it, or the program in its
    /// foreground (`zsh`, `vim`, `cargo`…).
    fn terminal_label(tab: &TerminalTab, cx: &App) -> String {
        tab.name
            .clone()
            .or_else(|| tab.view.read(cx).process_name().map(str::to_owned))
            .unwrap_or_else(shell_name)
    }

    /// Labels of the terminals, in list order (tests).
    #[cfg(test)]
    pub fn terminal_labels(&self, cx: &App) -> Vec<String> {
        self.terminals
            .tabs
            .iter()
            .map(|tab| Self::terminal_label(tab, cx))
            .collect()
    }

    /// The list of terminals on the right of the panel.
    fn render_terminal_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let width = self
            .layout
            .terminal_list_width
            .clamp(LIST_WIDTH.0, LIST_WIDTH.1);
        div()
            .relative()
            .w(px(width))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("resize-terminal-list")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(-4.))
                    .w(px(8.))
                    .cursor_col_resize()
                    .on_drag(TerminalListResize, |_, _, _, cx| cx.new(|_| NoPreview)),
            )
            .child(self.render_terminal_rows(cx))
    }

    fn render_terminal_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .id("terminal-list")
            .size_full()
            .flex()
            .flex_col()
            .py_1()
            .overflow_y_scroll()
            .children(self.terminals.tabs.iter().enumerate().map(|(index, tab)| {
                let active = index == self.terminals.active;
                let id = tab.view.entity_id();
                let group = gpui::SharedString::from(format!("terminal-tab-{index}"));
                div()
                    .id(("terminal-tab", index))
                    .group(group.clone())
                    .h(px(26.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .pl_2()
                    .pr_1()
                    .mx_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(if active {
                        colors.text
                    } else {
                        colors.text_muted
                    })
                    .when(active, |this| this.bg(colors.surface_hover))
                    .hover_style(move |s| s.bg(colors.surface_hover))
                    .tooltip(crate::ui::tooltip(
                        tab.view
                            .read(cx)
                            .title()
                            .filter(|title| !title.trim().is_empty())
                            .map_or_else(|| Self::terminal_label(tab, cx), str::to_owned),
                    ))
                    .child(
                        svg()
                            .data(ICON_TERMINAL)
                            .size(px(13.))
                            .flex_shrink_0()
                            .text_color(colors.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(Self::terminal_label(tab, cx)),
                    )
                    .child(
                        self.panel_icon_button(
                            ("terminal-tab-kill", index),
                            ICON_KILL,
                            "Kill Terminal",
                            cx,
                        )
                        .size(px(20.))
                        .invisible()
                        .group_hover_style(group, |style| style.visible())
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_terminal(id, window, cx)
                            },
                        )),
                    )
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            this.activate_terminal(index, window, cx);
                            if event.click_count() == 2 {
                                this.start_terminal_rename(id, event.position(), window, cx);
                            }
                        }),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            this.terminal_menu(id, event.position, window, cx)
                        }),
                    )
            }))
    }

    /// The terminal panel, when shown.
    pub(super) fn render_terminal_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.layout.terminal_visible {
            return None;
        }
        let colors = cx.theme().colors.clone();
        let active = self.terminals.active().cloned();
        let label = self
            .terminals
            .tabs
            .get(self.terminals.active)
            .map(|tab| Self::terminal_label(tab, cx));
        let maximized = self.terminal_maximized();
        let count = self.terminals.tabs.len();
        let body = match (&active, &self.terminals.error) {
            (Some(view), _) => div().size_full().child(view.clone()),
            (None, Some(error)) => div().p_3().text_color(colors.error).child(error.clone()),
            (None, None) => div(),
        };
        let header = div()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .text_size(px(11.))
            .text_color(colors.text_muted)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("TERMINAL"))
            .when_some(
                label
                    .zip(active.as_ref().map(|v| v.entity_id()))
                    .filter(|_| count < 2),
                |this, (label, id)| {
                    // Double-click or right-click to rename.
                    this.child(
                        div()
                            .id("terminal-label")
                            .min_w_0()
                            .truncate()
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(
                                move |this, event: &gpui::ClickEvent, window, cx| {
                                    if event.click_count() == 2 {
                                        this.start_terminal_rename(
                                            id,
                                            event.position(),
                                            window,
                                            cx,
                                        );
                                    }
                                },
                            ))
                            .on_mouse_down(
                                gpui::MouseButton::Right,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        this.terminal_menu(id, event.position, window, cx)
                                    },
                                ),
                            ),
                    )
                },
            )
            .child(div().flex_1())
            .child(
                self.panel_icon_button("terminal-new", ICON_NEW, "New Terminal (ctrl-shift-`)", cx)
                    .on_click(cx.listener(|this, _, window, cx| this.new_terminal(window, cx))),
            )
            .when_some(active.map(|view| view.entity_id()), |this, id| {
                this.child(
                    self.panel_icon_button("terminal-kill", ICON_KILL, "Kill Terminal", cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.close_terminal(id, window, cx)
                        })),
                )
            })
            .child(
                self.panel_icon_button(
                    "terminal-maximize",
                    if maximized {
                        ICON_RESTORE
                    } else {
                        ICON_MAXIMIZE
                    },
                    if maximized {
                        "Restore Panel Size"
                    } else {
                        "Maximize Panel Size"
                    },
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_terminal_maximized(cx))),
            )
            .child(
                self.panel_icon_button("terminal-hide", ICON_CLOSE, "Hide Panel", cx)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx))),
            );
        Some(
            div()
                .id("terminal-panel")
                .relative()
                .map(|this| {
                    if maximized {
                        this.flex_1().min_h_0()
                    } else {
                        this.h(px(self.layout.terminal_height)).flex_shrink_0()
                    }
                })
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.background)
                .child(header)
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .child(div().flex_1().min_w_0().h_full().child(body))
                        .when(count > 1, |this| this.child(self.render_terminal_list(cx))),
                )
                .when(!maximized, |this| {
                    this.child(
                        div()
                            .id("resize-terminal")
                            .absolute()
                            .top(px(-4.))
                            .left_0()
                            .right_0()
                            .h(px(8.))
                            .cursor_row_resize()
                            .on_drag(TerminalResize, |_, _, _, cx| cx.new(|_| NoPreview)),
                    )
                }),
        )
    }
}
