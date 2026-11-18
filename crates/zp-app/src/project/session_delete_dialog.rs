use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent, PromptLevel,
    PromptResponse, Render, Window, div, prelude::*, px, rgba,
};

use crate::theme::ActiveTheme;
use crate::ui::{button, checkbox};

const HIDE: usize = 0;
const CANCEL: usize = 1;
const TRASH: usize = 2;

pub(super) enum Decision {
    Hide,
    Trash,
}

pub(super) fn show(
    title: &str,
    window: &mut Window,
    cx: &mut App,
) -> impl Future<Output = Option<Decision>> + use<> {
    cx.set_prompt_builder(|_, _, detail, _, handle, window, cx| {
        let dialog = cx.new(|cx| DeleteSessionDialog {
            title: detail.unwrap_or_default().to_owned(),
            trash: false,
            focus: cx.focus_handle(),
        });
        handle.with_view(dialog, window, cx)
    });
    let answer = window.prompt(
        PromptLevel::Warning,
        "Delete session?",
        Some(title),
        &["Hide Session", "Cancel", "Move to Trash"],
        cx,
    );
    cx.reset_prompt_builder();
    async move {
        match answer.await {
            Ok(HIDE) => Some(Decision::Hide),
            Ok(TRASH) => Some(Decision::Trash),
            _ => None,
        }
    }
}

struct DeleteSessionDialog {
    title: String,
    trash: bool,
    focus: FocusHandle,
}

impl DeleteSessionDialog {
    fn confirm(&self, cx: &mut Context<Self>) {
        cx.emit(PromptResponse(if self.trash { TRASH } else { HIDE }));
    }

    fn toggle_trash(&mut self, cx: &mut Context<Self>) {
        self.trash = !self.trash;
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" => self.confirm(cx),
            "escape" => cx.emit(PromptResponse(CANCEL)),
            "space" => {
                if !event.is_held {
                    self.toggle_trash(cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl EventEmitter<PromptResponse> for DeleteSessionDialog {}

impl Focusable for DeleteSessionDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DeleteSessionDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let font_size = theme.metrics.font_size;
        let title_size = theme.metrics.dialog_title_size();
        let detail_size = theme.metrics.dialog_detail_size();
        div()
            .id("delete-session-dialog")
            .role(gpui::accesskit::Role::Dialog)
            .aria_label("Delete session?")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .bg(rgba(0x0000_0073))
            .child(
                div()
                    .w(px(440.))
                    .max_w_full()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .shadow_lg()
                    .text_color(colors.text)
                    .text_size(font_size)
                    .child(
                        div()
                            .debug_selector(|| "delete-session-title".into())
                            .text_size(title_size)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Delete session?"),
                    )
                    .child(div().truncate().child(self.title.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                checkbox(
                                    "delete-session-file",
                                    "Also move the session file to Trash",
                                    self.trash,
                                    cx,
                                )
                                .aria_keyshortcuts("Space")
                                .debug_selector(|| "delete-session-file".into())
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_trash(cx))),
                            )
                            .child(
                                div()
                                    .pl_5()
                                    .text_size(detail_size)
                                    .text_color(colors.text_muted)
                                    .child(
                                        "Moving the file to Trash can reduce local usage totals.",
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button(
                                    "cancel-session-delete",
                                    "Cancel",
                                    None,
                                    cx,
                                    cx.listener(|_, _, _, cx| cx.emit(PromptResponse(CANCEL))),
                                )
                                .debug_selector(|| "cancel-session-delete".into()),
                            )
                            .child(
                                button(
                                    "confirm-session-delete",
                                    if self.trash {
                                        "Move to Trash"
                                    } else {
                                        "Hide Session"
                                    },
                                    None,
                                    cx,
                                    cx.listener(|this, _, _, cx| this.confirm(cx)),
                                )
                                .debug_selector(|| "confirm-session-delete".into())
                                .when(self.trash, |this| this.text_color(colors.error)),
                            ),
                    ),
            )
    }
}
