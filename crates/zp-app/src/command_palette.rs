//! The command palette (`secondary-shift-p`): every command by name, with its shortcut.

use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, Window, div, prelude::*, px, rgb_to_hsla,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, TextChanged, text_input};

use crate::actions::{self, AcceptCompletion, CloseCompletion, CompletionNext, CompletionPrevious};
use crate::theme::ActiveTheme;

/// A command of the palette.
pub struct PaletteCommand {
    pub label: &'static str,
    pub action: Box<dyn Action>,
}

/// Every command, in display order.
pub fn commands() -> Vec<PaletteCommand> {
    let command = |label, action: Box<dyn Action>| PaletteCommand { label, action };
    vec![
        command("New Chat", Box::new(actions::NewChat)),
        command(
            "Compact Conversation",
            Box::new(actions::CompactConversation),
        ),
        command("Session Tree", Box::new(actions::ToggleSessionTree)),
        command("Next Tab", Box::new(actions::NextTab)),
        command("Previous Tab", Box::new(actions::PreviousTab)),
        command("Close Tab", Box::new(actions::CloseTab)),
        command("Toggle Sidebar", Box::new(actions::ToggleSidebar)),
        command("Toggle Changes Panel", Box::new(actions::ToggleRightPanel)),
        command(
            "Reload Templates, Skills and MCP Servers",
            Box::new(actions::ReloadResources),
        ),
        command("Settings", Box::new(actions::OpenSettings)),
        command("Open Folder…", Box::new(actions::OpenFolder)),
        command("New Window", Box::new(actions::NewWindow)),
        command("Close Window", Box::new(actions::CloseWindow)),
        command("Check for Updates…", Box::new(actions::CheckForUpdates)),
        command("About ZP", Box::new(actions::About)),
        command("Quit", Box::new(actions::Quit)),
    ]
}

/// Events of the palette.
pub enum PaletteEvent {
    /// Run this action.
    Run(Box<dyn Action>),
}

pub struct CommandPalette {
    filter: Entity<EditableTextState>,
    selected: usize,
    _subscription: gpui::Subscription,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}
impl EventEmitter<DismissEvent> for CommandPalette {}

impl CommandPalette {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        filter.read(cx).focus_handle(cx).focus(window, cx);
        let subscription = cx.subscribe(&filter, |this, _, _: &TextChanged, cx| {
            this.selected = 0;
            cx.notify();
        });
        Self {
            filter,
            selected: 0,
            _subscription: subscription,
        }
    }

    /// The filter field (tests).
    #[cfg(test)]
    pub fn filter(&self) -> &Entity<EditableTextState> {
        &self.filter
    }

    /// Commands matching the filter: (label, action name).
    pub fn matches(&self, cx: &App) -> Vec<(usize, &'static str)> {
        let query = self.filter.read(cx).as_str().trim().to_lowercase();
        let mut scored: Vec<(i64, usize, &'static str)> = commands()
            .iter()
            .enumerate()
            .filter_map(|(index, c)| {
                let score = zp_core::attachments::fuzzy_score(c.label, &query)?;
                Some((score, index, c.label))
            })
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        scored
            .into_iter()
            .map(|(_, index, label)| (index, label))
            .collect()
    }

    fn run(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let matches = self.matches(cx);
        let Some((command, _)) = matches.get(index.unwrap_or(self.selected)) else {
            return;
        };
        if let Some(command) = commands().into_iter().nth(*command) {
            cx.emit(PaletteEvent::Run(command.action));
        }
    }

    /// Runs the selected command (tests and Enter).
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        self.run(None, cx);
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.matches(cx).len() as isize;
        if count > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
            cx.notify();
        }
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.read(cx).focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let matches = self.matches(cx);
        let actions = commands();
        div()
            .key_context("CommandPalette")
            .on_action(cx.listener(|this, _: &CompletionNext, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &CompletionPrevious, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &AcceptCompletion, _, cx| this.confirm(cx)))
            .on_action(cx.listener(|_, _: &CloseCompletion, _, cx| cx.emit(DismissEvent)))
            .occlude()
            .w(px(520.))
            .flex()
            .flex_col()
            .p_1()
            .rounded(px(10.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(
                text_input("palette-filter")
                    .state(self.filter.downgrade())
                    .placeholder("Type a command…")
                    .placeholder_color(rgb_to_hsla(colors.text_muted))
                    .caret_color(rgb_to_hsla(colors.text))
                    .w_full()
                    .px_3()
                    .py_2()
                    .whitespace_nowrap(),
            )
            .child(div().h(px(1.)).bg(colors.border))
            .child(
                div()
                    .id("palette-items")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .py_1()
                    .when(matches.is_empty(), |this| {
                        this.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_color(colors.text_muted)
                                .child("No matching commands"),
                        )
                    })
                    .children(
                        matches
                            .into_iter()
                            .enumerate()
                            .map(|(row, (index, label))| {
                                let keys = actions
                                    .get(index)
                                    .and_then(|c| actions::shortcut_keys(c.action.name(), cx))
                                    .map(|keys| crate::ui::shortcut_label(&keys));
                                div()
                                    .id(("palette-item", row))
                                    .flex()
                                    .justify_between()
                                    .px_3()
                                    .py_1()
                                    .mx_1()
                                    .rounded(px(5.))
                                    .cursor_pointer()
                                    .when(row == self.selected, |this| {
                                        this.bg(colors.surface_hover)
                                    })
                                    .hover_style(|s| s.bg(colors.surface_hover))
                                    .child(label)
                                    .children(keys.map(|keys| {
                                        div()
                                            .text_size(px(12.))
                                            .text_color(colors.text_muted)
                                            .child(keys)
                                    }))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.run(Some(row), cx)),
                                    )
                            }),
                    ),
            )
    }
}
