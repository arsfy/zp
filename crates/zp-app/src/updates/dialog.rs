//! The in-window dialog of update prompts: the ZP logo, a title, details and buttons.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, PromptButton, PromptHandle,
    PromptLevel, PromptResponse, Render, RenderablePromptHandle, Window, div, img, prelude::*, px,
    rgba,
};

use crate::theme::ActiveTheme;

/// A [`gpui::App::set_prompt_builder`] renderer. The first answer is the primary action;
/// Escape picks the second ("Later", "Cancel") or the only one.
pub fn build(
    level: PromptLevel,
    title: &str,
    detail: Option<&str>,
    answers: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let dialog = cx.new(|cx| UpdateDialog {
        level,
        title: title.to_owned(),
        detail: detail.unwrap_or_default().to_owned(),
        answers: answers.iter().map(|a| a.label().to_string()).collect(),
        focus: cx.focus_handle(),
    });
    handle.with_view(dialog, window, cx)
}

pub struct UpdateDialog {
    level: PromptLevel,
    title: String,
    detail: String,
    answers: Vec<String>,
    focus: FocusHandle,
}

impl UpdateDialog {
    fn dismiss_answer(&self) -> usize {
        if self.answers.len() > 1 { 1 } else { 0 }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let answer = match event.keystroke.key.as_str() {
            "enter" => 0,
            "escape" => self.dismiss_answer(),
            _ => return,
        };
        cx.stop_propagation();
        cx.emit(PromptResponse(answer));
    }

    fn render_button(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let primary = index == 0 && self.answers.len() > 1 || self.answers.len() == 1;
        div()
            .id(("update-dialog-answer", index))
            .px_3()
            .py_1p5()
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(13.))
            .border_1()
            .when(primary, |this| {
                this.bg(colors.accent)
                    .border_color(colors.accent)
                    .text_color(colors.accent_text)
                    .font_weight(gpui::FontWeight::MEDIUM)
            })
            .when(!primary, |this| {
                this.border_color(colors.border)
                    .bg(colors.background)
                    .hover_style(move |s| s.bg(colors.surface_hover))
            })
            .child(self.answers[index].clone())
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(PromptResponse(index));
            }))
    }
}

impl EventEmitter<PromptResponse> for UpdateDialog {}

impl Focusable for UpdateDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for UpdateDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let accent = match self.level {
            PromptLevel::Info => None,
            PromptLevel::Warning => Some(colors.warning),
            PromptLevel::Critical => Some(colors.error),
        };
        // Secondary answers (e.g. "Release Notes") on the left, then dismiss and primary.
        let count = self.answers.len();
        let (leading, trailing): (Vec<usize>, Vec<usize>) =
            (0..count).rev().partition(|index| *index >= 2);
        let leading: Vec<_> = leading
            .into_iter()
            .map(|index| self.render_button(index, cx).into_any_element())
            .collect();
        let trailing: Vec<_> = trailing
            .into_iter()
            .map(|index| self.render_button(index, cx).into_any_element())
            .collect();
        let paragraphs: Vec<String> = self
            .detail
            .split("\n\n")
            .map(str::to_owned)
            .filter(|p| !p.trim().is_empty())
            .collect();
        div()
            .id("update-dialog")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0073))
            .child(
                div()
                    .w(px(460.))
                    .max_w_full()
                    .mx_4()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .shadow_lg()
                    .text_color(colors.text)
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_4()
                            .child(div().size(px(56.)).flex_shrink_0().children(
                                crate::ui::app_logo(cx).map(|logo| img(logo).size(px(56.))),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(16.))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .when_some(accent, |this, color| this.text_color(color))
                                            .child(self.title.clone()),
                                    )
                                    .children(paragraphs.into_iter().enumerate().map(
                                        |(index, paragraph)| {
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap_0p5()
                                                .text_size(px(13.))
                                                .text_color(if index == 0 {
                                                    colors.text
                                                } else {
                                                    colors.text_muted
                                                })
                                                .children(
                                                    paragraph
                                                        .lines()
                                                        .map(|line| div().child(line.to_owned()))
                                                        .collect::<Vec<_>>(),
                                                )
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(leading)
                            .child(div().flex_1())
                            .children(trailing),
                    ),
            )
    }
}
