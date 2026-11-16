//! Rendering of the conversation: transcript rows, tool cards and the composer.

use std::time::Duration;

use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, ClipboardItem, ColorExt as _, Context,
    ElementId, FontWeight, IntoElement, Render, SharedString, Window, anchored, canvas, deferred,
    div, linear_color_stop, linear_gradient, list, point, prelude::*, px, relative, rgb_to_hsla,
    svg,
};
use gpui_elements::editable_text::{text_area, text_input};
use zp_ai::{AssistantContent, StopReason, ToolCall};
use zp_core::checkpoint::RecordedChange;

use super::permissions::permission_mode_color;
use super::transcript::{Row, ToolStatus};
use super::{ConversationEvent, ConversationView, Picker};
use crate::actions::KEY_SEND;
use crate::diff_view::{DiffColors, render_unified_row};
use crate::markdown::{IdGen, MarkdownStyle, render_markdown};
use crate::theme::ActiveTheme;

/// Readable width of the transcript and composer.
const CONTENT_WIDTH: f32 = 820.0;
/// Lines of tool output shown in an expanded card.
const OUTPUT_PREVIEW_LINES: usize = 30;
/// Diff rows shown under a tool card.
const INLINE_DIFF_ROWS: usize = 40;
/// Models listed at once in the picker.
const PICKER_LIMIT: usize = 200;

pub(super) fn id(name: impl Into<SharedString>, index: usize) -> ElementId {
    ElementId::NamedInteger(name.into(), index as u64)
}

/// Centers content at the readable width.
fn centered(content: impl IntoElement) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .justify_center()
        .px_4()
        .child(div().w_full().max_w(px(CONTENT_WIDTH)).child(content))
}

/// Three dots pulsing in turn: the agent is working.
fn loading_dots(cx: &App) -> AnyElement {
    let color = cx.theme().colors.text_muted;
    div()
        .id("loading")
        .h(px(20.))
        .flex()
        .items_center()
        .gap(px(5.))
        .children((0..3usize).map(|index| {
            div().size(px(6.)).rounded_full().bg(color).with_animation(
                ("loading-dot", index),
                Animation::new(Duration::from_millis(1200)).repeat_synced(),
                move |dot, t| {
                    // Each dot peaks a third of a cycle after the previous one.
                    let phase = (t - index as f32 / 3.0).rem_euclid(1.0);
                    let wave = (phase * std::f32::consts::TAU).cos() * 0.5 + 0.5;
                    dot.opacity(0.25 + 0.75 * wave)
                },
            )
        }))
        .into_any_element()
}

/// Height of the fade above the floating composer.
const COMPOSER_FADE: f32 = 24.0;
/// Height of the line under a message (actions or the working indicator).
const ACTION_SIZE: f32 = 22.0;
const ICON_COPY: &[u8] = include_bytes!("../../../../assets/icons/copy.svg");
const ICON_CHECK: &[u8] = include_bytes!("../../../../assets/icons/check.svg");
const ICON_EDIT: &[u8] = include_bytes!("../../../../assets/icons/pencil.svg");
const ICON_BRANCH: &[u8] = include_bytes!("../../../../assets/icons/git-branch.svg");
const ICON_FORK: &[u8] = include_bytes!("../../../../assets/icons/square-arrow-out-up-right.svg");

/// A small square button showing an icon, with a tooltip.
fn icon_button(
    element_id: impl Into<ElementId>,
    icon: &'static [u8],
    label: &'static str,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    div()
        .id(element_id)
        .size(px(ACTION_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .hover_style(move |s| s.bg(colors.surface_hover))
        .tooltip(crate::ui::tooltip(label))
        .child(svg().data(icon).size(px(13.)).text_color(colors.text_muted))
}

fn ghost_button(
    element_id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    ghost_button_with_color(element_id, label, None, cx)
}

fn ghost_button_with_color(
    element_id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    color: Option<gpui::Rgba>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = &cx.theme().colors;
    div()
        .id(element_id)
        .px_2()
        .py_0p5()
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(color.unwrap_or(colors.text_muted))
        .cursor_pointer()
        .hover_style(|s| {
            s.bg(colors.surface_hover)
                .text_color(color.unwrap_or(colors.text))
        })
        .child(label.into())
}

impl ConversationView {
    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if ix >= self.transcript.len() {
            // Room for the floating composer at the end.
            return div()
                .h(px(self.composer_height.get() + COMPOSER_FADE))
                .into_any_element();
        }
        let row = self.transcript.rows().get(ix).cloned();
        let content = match &row {
            Some(Row::User { .. }) if self.editing().is_some_and(|edit| edit.row == ix) => {
                self.render_user_editor(cx)
            }
            Some(Row::User {
                text,
                images,
                files,
                ..
            }) => self.render_user(text, *images, files, cx),
            Some(Row::Assistant { .. }) => self.render_assistant(ix, cx),
            Some(Row::Notice { kind, text, .. }) if kind == zp_session::COMPACTION_SUMMARY_TYPE => {
                let colors = cx.theme().colors.clone();
                let key = format!("compaction-{ix}");
                let expanded = self.tool_expanded.get(&key).copied().unwrap_or(false);
                let toggle_key = key.clone();
                let summary = expanded.then(|| {
                    let blocks = self.parsed_markdown(text);
                    let mut ids = IdGen::new(format!("compaction-md-{ix}"));
                    render_markdown(&blocks, &MarkdownStyle::from_theme(cx), &mut ids)
                });
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .child(
                        div()
                            .id(ElementId::Name(key.into()))
                            .flex()
                            .gap_2()
                            .px_3()
                            .py_1p5()
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .hover_style(|s| s.bg(colors.surface_hover))
                            .child(if expanded { "▾" } else { "▸" })
                            .child("Earlier messages were summarized to save context")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_tool(&toggle_key, ix, expanded, cx)
                            })),
                    )
                    .when_some(summary, |this, summary| {
                        this.child(
                            div()
                                .px_3()
                                .pb_2()
                                .pt_1()
                                .border_t_1()
                                .border_color(colors.border)
                                .child(summary),
                        )
                    })
                    .into_any_element()
            }
            Some(Row::Notice { kind, text, .. }) if kind == zp_core::changes::FILE_RESTORE_TYPE => {
                let colors = &cx.theme().colors;
                div()
                    .flex()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded(px(6.))
                    .bg(colors.surface)
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(div().text_color(colors.warning).child("↺"))
                    .child(text.clone())
                    .into_any_element()
            }
            Some(Row::Notice { kind, text, .. }) => {
                let colors = &cx.theme().colors;
                div()
                    .px_3()
                    .py_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .text_color(colors.text_muted)
                    .italic()
                    .child(format!("[{kind}] {text}"))
                    .into_any_element()
            }
            None => div().into_any_element(),
        };
        let footer = row.and_then(|row| self.render_row_footer(ix, &row, cx));
        centered(
            div()
                .py_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(content)
                .children(footer),
        )
        .into_any_element()
    }

    /// Whether row `ix` ends a turn (the next row is not part of the same answer).
    fn ends_turn(&self, ix: usize) -> bool {
        !matches!(
            self.transcript.rows().get(ix + 1),
            Some(Row::Assistant { .. })
        )
    }

    /// The line under a message: the working indicator on the last row while busy,
    /// otherwise its actions. It has a fixed height so rows never move when it changes;
    /// assistant messages in the middle of an answer have none.
    fn render_row_footer(
        &self,
        ix: usize,
        row: &Row,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let last = ix + 1 == self.transcript.len();
        let is_user = match row {
            Row::User { .. } => true,
            Row::Assistant { .. } => false,
            Row::Notice { .. } => return None,
        };
        if !is_user && !self.ends_turn(ix) {
            return None;
        }
        let slot = div().h(px(ACTION_SIZE)).flex().items_center();
        if self.editing().is_some_and(|edit| edit.row == ix) {
            return Some(slot.into_any_element());
        }
        if self.is_busy() {
            return Some(
                slot.when(last, |this| this.child(loading_dots(cx)))
                    .into_any_element(),
            );
        }
        let Some(actions) = self.render_row_actions(ix, row, cx) else {
            return Some(slot.into_any_element());
        };
        Some(
            slot.when(is_user, |this| this.justify_end())
                .child(actions)
                .into_any_element(),
        )
    }

    /// Icon buttons for a message: edit or branch, fork, copy.
    fn render_row_actions(
        &self,
        ix: usize,
        row: &Row,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        self.coding.as_ref()?;
        let (text, is_user) = match row {
            Row::User { text, .. } => (text.clone(), true),
            Row::Assistant { .. } => (self.transcript.assistant(ix)?.text(), false),
            Row::Notice { .. } => return None,
        };
        let mut bar = div().flex().items_center().gap_0p5();
        if !text.trim().is_empty() {
            let copied = self.copied_row == Some(ix);
            bar = bar.child(
                icon_button(
                    id("copy", ix),
                    if copied { ICON_CHECK } else { ICON_COPY },
                    if copied { "Copied" } else { "Copy" },
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    this.show_copied(ix, cx);
                })),
            );
        }
        if is_user {
            bar = bar.child(
                icon_button(id("edit", ix), ICON_EDIT, "Edit & Resend", cx).on_click(
                    cx.listener(move |this, _, window, cx| this.edit_and_resend(ix, window, cx)),
                ),
            );
        } else {
            bar = bar.child(
                icon_button(id("branch", ix), ICON_BRANCH, "Branch from here", cx).on_click(
                    cx.listener(move |this, _, window, cx| this.branch_from(ix, window, cx)),
                ),
            );
        }
        bar = bar.child(
            icon_button(id("fork", ix), ICON_FORK, "Fork to new tab", cx)
                .on_click(cx.listener(move |this, _, _, cx| this.fork_from(ix, cx))),
        );
        Some(bar)
    }

    /// A sent message being edited: the full width, with Cancel and Send.
    fn render_user_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(edit) = self.editing() else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors.clone();
        div()
            .key_context("MessageEditor")
            .on_action(cx.listener(|this, _: &crate::actions::SubmitEdit, window, cx| {
                this.submit_edit(window, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::actions::CancelEdit, window, cx| {
                this.cancel_edit(window, cx)
            }))
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(px(10.))
            .bg(colors.surface)
            .child(
                text_area("message-editor")
                    .state(edit.input.downgrade())
                    .caret_color(rgb_to_hsla(colors.text))
                    .selection_color(rgb_to_hsla(colors.accent).opacity(0.35))
                    .w_full()
                    .max_h(px(320.))
                    .overflow_y_scroll(),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(colors.text_muted)
                            .child("Sending starts a new branch from here; the original stays in the session tree."),
                    )
                    .child(
                        ghost_button("edit-cancel", "Cancel", cx).on_click(cx.listener(
                            |this, _, window, cx| this.cancel_edit(window, cx),
                        )),
                    )
                    .child(
                        div()
                            .id("edit-send")
                            .px_3()
                            .py_0p5()
                            .rounded(px(4.))
                            .text_size(px(12.))
                            .bg(colors.accent)
                            .text_color(colors.accent_text)
                            .cursor_pointer()
                            .child("Send")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_edit(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The user's message: a bubble as wide as its text, on the right.
    fn render_user(&self, text: &str, images: usize, files: &[String], cx: &App) -> AnyElement {
        let colors = &cx.theme().colors;
        let bubble = div()
            .max_w(relative(0.85))
            .flex()
            .flex_col()
            .gap_0p5()
            .px_3()
            .py_2()
            .rounded(px(10.))
            .bg(colors.surface)
            .child(text.to_owned())
            .when(!files.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child(format!("Attached: {}", files.join(", "))),
                )
            })
            .when(images > 0, |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child(format!("{images} image(s) attached")),
                )
            });
        div().flex().justify_end().child(bubble).into_any_element()
    }

    fn render_assistant(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(message) = self.transcript.assistant(row).cloned() else {
            return div().into_any_element();
        };
        let streaming = self.transcript.is_streaming(row);
        let colors = cx.theme().colors.clone();
        let style = MarkdownStyle::from_theme(cx);
        let mut column = div().flex().flex_col().gap_2();
        let last_block = message.content.len().saturating_sub(1);
        for (block, content) in message.content.iter().enumerate() {
            column = match content {
                AssistantContent::Text(text) if text.text.trim().is_empty() => column,
                AssistantContent::Text(text) => {
                    let blocks = self.parsed_markdown(&text.text);
                    let mut ids = IdGen::new(format!("md-{row}-{block}"));
                    column.child(render_markdown(&blocks, &style, &mut ids))
                }
                AssistantContent::Thinking(thinking) => column.child(self.render_thinking(
                    row,
                    block,
                    &thinking.thinking,
                    streaming && block == last_block,
                    cx,
                )),
                AssistantContent::ToolCall(call) => column.child(self.render_tool(row, call, cx)),
            };
        }
        if !streaming && matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            let aborted = message.stop_reason == StopReason::Aborted;
            let can_retry = self.transcript.failed_response() == Some(row) && !self.running;
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if aborted { colors.border } else { colors.error })
                    .child(
                        div()
                            .flex_1()
                            .text_color(if aborted {
                                colors.text_muted
                            } else {
                                colors.error
                            })
                            .child(if aborted {
                                "Stopped.".to_owned()
                            } else {
                                format!(
                                    "Error: {}",
                                    message.error_message.as_deref().unwrap_or("request failed")
                                )
                            }),
                    )
                    .when(can_retry, |this| {
                        this.child(
                            ghost_button(id("retry", row), "Retry", cx)
                                .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                        )
                    }),
            );
        }
        column.into_any_element()
    }

    fn render_thinking(
        &self,
        row: usize,
        block: usize,
        text: &str,
        live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let expanded = self.expanded_thinking.contains(&(row, block));
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .id(ElementId::NamedInteger(
                        format!("thinking-{row}").into(),
                        block as u64,
                    ))
                    .flex()
                    .gap_1()
                    .text_color(colors.text_muted)
                    .italic()
                    .cursor_pointer()
                    .child(if expanded { "▾" } else { "▸" })
                    .child(if live { "Thinking…" } else { "Thinking" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_thinking_block(row, block, cx)
                    })),
            )
            .when(expanded, |this| {
                this.child(
                    div()
                        .pl_3()
                        .border_l_2()
                        .border_color(colors.border)
                        .text_color(colors.text_muted)
                        .child(text.to_owned()),
                )
            })
            .into_any_element()
    }

    fn render_tool(&self, row: usize, call: &ToolCall, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let mono = cx.theme().metrics.mono_font.clone();
        let state = self.transcript.tool(&call.id);
        let status = state.map(|state| state.status);
        let change = state
            .filter(|state| state.status == ToolStatus::Done)
            .and_then(|state| state.details.as_ref())
            .and_then(RecordedChange::from_details);
        // File changes show their diff by default.
        let default_expanded = matches!(status, Some(ToolStatus::Error))
            || change.is_some()
            || (call.name == "bash" && matches!(status, Some(ToolStatus::Running)));
        let expanded = self
            .tool_expanded
            .get(&call.id)
            .copied()
            .unwrap_or(default_expanded);

        let approval = self.render_approval(&call.id, cx);
        let (status_text, status_color) = match status {
            _ if approval.is_some() => ("awaiting approval".to_owned(), colors.warning),
            None => ("pending".to_owned(), colors.text_muted),
            Some(ToolStatus::Running) => ("running…".to_owned(), colors.accent),
            Some(ToolStatus::Done) => match &change {
                Some(change) => (
                    format!("+{} −{}", change.added, change.removed),
                    colors.success,
                ),
                None => ("done".to_owned(), colors.success),
            },
            Some(ToolStatus::Error) => ("failed".to_owned(), colors.error),
        };

        let call_id = call.id.clone();
        let header = div()
            .id(ElementId::Name(format!("tool-{}", call.id).into()))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .cursor_pointer()
            .hover_style(|s| s.bg(colors.surface_hover))
            .child(
                div()
                    .text_color(colors.text_muted)
                    .child(if expanded { "▾" } else { "▸" }),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(call.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(mono.clone())
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(zp_tools::describe_call(&call.name, &call.arguments, 160)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(status_color)
                    .child(status_text),
            )
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_tool(&call_id, row, expanded, cx)),
            );

        let diff_body = change
            .as_ref()
            .filter(|_| expanded)
            .map(|change| self.render_inline_diff(&call.id, change, cx));
        let body = (expanded && diff_body.is_none() && approval.is_none()).then(|| {
            let output = state.map(|state| state.output.as_str()).unwrap_or_default();
            let lines: Vec<&str> = output.lines().collect();
            let (shown, note) = if lines.len() <= OUTPUT_PREVIEW_LINES {
                (lines.join("\n"), None)
            } else if call.name == "bash" {
                let hidden = lines.len() - OUTPUT_PREVIEW_LINES;
                (
                    lines[hidden..].join("\n"),
                    Some(format!("… {hidden} earlier lines")),
                )
            } else {
                (
                    lines[..OUTPUT_PREVIEW_LINES].join("\n"),
                    Some(format!(
                        "… {} more lines",
                        lines.len() - OUTPUT_PREVIEW_LINES
                    )),
                )
            };
            let empty = shown.trim().is_empty();
            div()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.code_background)
                .font_family(mono)
                .text_size(px(12.))
                .when_some(note.filter(|_| call.name == "bash"), |this, note| {
                    this.child(div().text_color(colors.text_muted).child(note))
                })
                .child(if empty {
                    div().text_color(colors.text_muted).child(
                        if matches!(status, Some(ToolStatus::Running)) {
                            "…"
                        } else {
                            "(no output)"
                        },
                    )
                } else {
                    div()
                        .when(matches!(status, Some(ToolStatus::Error)), |this| {
                            this.text_color(colors.error)
                        })
                        .child(shown)
                })
        });

        div()
            .flex()
            .flex_col()
            .rounded(px(6.))
            .border_1()
            .border_color(if matches!(status, Some(ToolStatus::Error)) {
                colors.error
            } else {
                colors.border
            })
            .bg(colors.surface)
            .overflow_hidden()
            .when(approval.is_some(), |this| this.border_color(colors.warning))
            .child(header)
            .children(approval)
            .children(diff_body)
            .children(body)
            .into_any_element()
    }

    /// The diff of a file change under its tool card (the first lines; the rest in the
    /// diff view).
    fn render_inline_diff(
        &self,
        call_id: &str,
        change: &RecordedChange,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let diff_colors = DiffColors::of(cx);
        let footer = |hidden: Option<String>| {
            let call_id = call_id.to_owned();
            let change = change.clone();
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_1()
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .child(div().children(hidden))
                .child(
                    div()
                        .id(ElementId::Name(format!("open-diff-{call_id}").into()))
                        .px_1p5()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover_style(|s| s.bg(colors.surface_hover).text_color(colors.text))
                        .child("Open diff")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(item) = this.tool_diff_item(&call_id, &change) {
                                cx.emit(ConversationEvent::OpenDiff(item));
                            }
                        })),
                )
        };
        let content = match &*self.tool_diff(call_id, change) {
            Err(error) => div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .child(format!("The diff is not available: {error}"))
                .into_any_element(),
            Ok(tool_diff) if tool_diff.diff.too_large => div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .child("The file is too large to compare.")
                .into_any_element(),
            Ok(tool_diff) => {
                let hidden = tool_diff.rows.len().saturating_sub(INLINE_DIFF_ROWS);
                div()
                    .flex()
                    .flex_col()
                    .py_1()
                    .children(
                        tool_diff
                            .rows
                            .iter()
                            .take(INLINE_DIFF_ROWS)
                            .map(|row| render_unified_row(row, &diff_colors)),
                    )
                    .child(footer(
                        (hidden > 0).then(|| format!("… {hidden} more lines")),
                    ))
                    .into_any_element()
            }
        };
        div()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.code_background)
            .child(content)
            .into_any_element()
    }

    // ----- composer --------------------------------------------------------------------

    fn render_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let current = self.model().map(|model| model.qualified_id());
        let choices = self.filtered_models(cx);
        let total = choices.len();
        let items = choices
            .into_iter()
            .take(PICKER_LIMIT)
            .enumerate()
            .map(|(index, choice)| {
                let selected = current.as_deref() == Some(choice.model.qualified_id().as_str());
                let model = choice.model.clone();
                let context = if model.context_window >= 1000 {
                    format!("{}k", model.context_window / 1000)
                } else {
                    String::new()
                };
                div()
                    .id(id("model", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover_style(|s| s.bg(colors.surface_hover))
                    .child(div().w(px(12.)).child(if selected { "✓" } else { "" }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(choice.model.qualified_id()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(colors.text_muted)
                            .child(format!("{} {context}", choice.provider_name)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_model(model.clone(), window, cx)
                    }))
            });
        let list = div()
            .id("model-list")
            .flex()
            .flex_col()
            .max_h(px(320.))
            .overflow_y_scroll()
            .px_1()
            .pb_1()
            .children(items)
            .when(total == 0, |this| {
                let none_available = self.model_choices(cx).is_empty();
                this.child(div().px_2().py_1().text_color(colors.text_muted).child(
                    if none_available {
                        "No available models"
                    } else {
                        "No matching models"
                    },
                ))
                .when(none_available, |this| {
                    this.child(
                        div()
                            .id("picker-add-key")
                            .mx_2()
                            .mb_1()
                            .cursor_pointer()
                            .text_color(colors.accent)
                            .hover_style(|s| s.text_color(colors.text))
                            .child("Add a provider…")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.picker = None;
                                this.add_api_key(None, cx);
                            })),
                    )
                })
            })
            .when(total > PICKER_LIMIT, |this| {
                this.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child(format!("{} more — refine the filter", total - PICKER_LIMIT)),
                )
            });
        self.popover(
            div()
                .w(px(420.))
                .child(
                    div().key_context("ModelPicker").p_2().child(
                        text_input("model-filter")
                            .state(self.model_filter.downgrade())
                            .placeholder("Filter models…")
                            .placeholder_color(rgb_to_hsla(colors.text_muted))
                            .caret_color(rgb_to_hsla(colors.text))
                            .px_2()
                            .py_1()
                            .w_full()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(colors.border)
                            .whitespace_nowrap(),
                    ),
                )
                .child(list),
            cx,
        )
    }

    fn render_thinking_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        if self.coding.is_none() {
            return div().into_any_element();
        }
        let current = self.effective_thinking();
        let items = self
            .thinking_levels()
            .into_iter()
            .enumerate()
            .map(|(index, level)| {
                div()
                    .id(id("thinking-level", index))
                    .flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover_style(|s| s.bg(colors.surface_hover))
                    .child(
                        div()
                            .w(px(12.))
                            .child(if level == current { "✓" } else { "" }),
                    )
                    .child(level.as_str())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_thinking(level, window, cx)
                    }))
            });
        self.popover(div().w(px(160.)).p_1().children(items), cx)
    }

    /// Wraps menu content in a floating panel that opens upward from its anchor.
    pub(super) fn popover(&self, content: gpui::Div, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        deferred(
            anchored()
                .anchor(Anchor::BottomLeft)
                .offset(point(px(0.), px(-6.)))
                .snap_to_window_with_margin(px(8.))
                .child(
                    content
                        .occlude()
                        .bg(colors.surface)
                        .border_1()
                        .border_color(colors.border)
                        .rounded(px(8.))
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.picker = None;
                            this.picker_closed_at = Some(std::time::Instant::now());
                            cx.notify();
                        })),
                ),
        )
        .priority(1)
        .into_any_element()
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let Some(coding) = &self.coding else {
            return div().into_any_element();
        };
        let model = self.model();
        let thinking = self.effective_thinking();
        let queued = coding.agent().steering_queue().len();
        let none_available = self.model_choices(cx).is_empty();
        // The session's model may belong to a provider without a key.
        let model_label = match &model {
            Some(model) if crate::store::AppModels::has_credentials(&model.provider, cx) => {
                model.qualified_id()
            }
            _ if none_available => "No available models".to_owned(),
            Some(model) => format!("{} · no key", model.qualified_id()),
            None => "Choose a model".to_owned(),
        };

        let picker_button = |picker: Picker, label: String, cx: &mut Context<Self>| {
            let open = self.picker == Some(picker);
            let color = match picker {
                Picker::Permissions => permission_mode_color(self.permission_mode(), cx),
                _ => None,
            };
            div()
                .relative()
                // Placed before the button so the popover anchors at its top and opens upward.
                .when(open, |this| {
                    this.child(match picker {
                        Picker::Model => self.render_model_picker(cx),
                        Picker::Thinking => self.render_thinking_picker(cx),
                        Picker::Permissions => self.render_permission_picker(cx),
                    })
                })
                .child(
                    ghost_button_with_color(
                        match picker {
                            Picker::Model => "model-button",
                            Picker::Thinking => "thinking-button",
                            Picker::Permissions => "permissions-button",
                        },
                        format!("{label} ▾"),
                        color,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_picker(picker, window, cx)
                    })),
                )
        };

        let action = if self.running {
            ghost_button("stop", "Stop", cx)
                .text_color(colors.error)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.stop(&crate::actions::StopRun, window, cx)
                }))
        } else {
            div()
                .id("send")
                .px_3()
                .py_0p5()
                .rounded(px(4.))
                .text_size(px(12.))
                .bg(colors.accent)
                .text_color(colors.accent_text)
                .cursor_pointer()
                .child("Send")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.send(&crate::actions::SendMessage, window, cx)
                }))
        };

        let notes = div()
                .flex()
                .flex_col()
                .gap_1()
                .when_some(self.run_error.clone(), |this, error| {
                    this.child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.error)
                            .child(error),
                    )
                })
                .when_some(self.needs_key(cx), |this, provider| {
                    let registered = cx
                        .try_global::<crate::store::AppModels>()
                        .and_then(|app| app.models.provider(&provider));
                    let name = registered
                        .as_ref()
                        .map_or_else(|| provider.clone(), |p| p.name().to_owned());
                    let (message, action) = match registered.as_ref().map(|p| (p.oauth().is_some(), p.accepts_api_key())) {
                        Some((true, true)) => (
                            format!("{name} needs a sign-in or an API key before ZP can send this message. Connect it, or pick another model."),
                            "Connect…",
                        ),
                        Some((true, false)) => (
                            format!("Sign in to {name} before ZP can send this message, or pick another model."),
                            "Sign In…",
                        ),
                        _ => (
                            format!("{name} needs an API key before ZP can send this message. Add one, or pick another model."),
                            "Add API Key…",
                        ),
                    };
                    this.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1p5()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.warning)
                            .text_size(px(12.))
                            .child(div().flex_1().child(message))
                            .child(
                                ghost_button("add-api-key", action, cx)
                                    .text_color(colors.accent)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.add_api_key(Some(provider.clone()), cx)
                                    })),
                            ),
                    )
                })
                .when(self.needs_model(), |this| {
                    let (message, button) = if none_available {
                        (
                            "No model is available yet. Add a provider to start the conversation.",
                            "Add a Provider…",
                        )
                    } else {
                        ("Choose a model before ZP can send this message.", "Choose a Model…")
                    };
                    this.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1p5()
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.warning)
                            .text_size(px(12.))
                            .child(div().flex_1().child(message))
                            .child(
                                ghost_button("choose-model", button, cx)
                                    .text_color(colors.accent)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if none_available {
                                            this.add_api_key(None, cx);
                                        } else {
                                            this.toggle_picker(Picker::Model, window, cx);
                                        }
                                    })),
                            ),
                    )
                })
                .when_some(self.info.clone(), |this, info| {
                    this.child(
                        div()
                            .id("composer-info")
                            .flex()
                            .gap_2()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(div().flex_1().whitespace_normal().child(info))
                            .child(
                                div()
                                    .id("dismiss-info")
                                    .cursor_pointer()
                                    .hover_style(|s| s.text_color(colors.text))
                                    .child("×")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.info = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                })
                .when(self.compacting, |this| {
                    this.child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.accent)
                            .child("Compacting the conversation…"),
                    )
                })
                .when(queued > 0, |this| {
                    this.child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(format!(
                                "{queued} queued message(s) — sent after the current step"
                            )),
                    )
                })
;
        let input = div()
            .key_context(if self.completion.is_some() {
                "Composer CompletionMenu"
            } else {
                "Composer"
            })
            .capture_action(cx.listener(
                |this, _: &gpui_elements::editable_text::actions::NavUp, window, cx| {
                    if this.recall_prompt(true, window, cx) {
                        cx.stop_propagation();
                    }
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_elements::editable_text::actions::NavDown, window, cx| {
                    if this.recall_prompt(false, window, cx) {
                        cx.stop_propagation();
                    }
                },
            ))
            .capture_action(cx.listener(
                |this, _: &gpui_elements::editable_text::actions::Paste, _, cx| {
                    if this.paste_attachments(cx) {
                        cx.stop_propagation();
                    }
                },
            ))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .when(!self.attachments.is_empty(), |this| {
                this.child(div().flex().flex_wrap().gap_1p5().px_3().pt_2().children(
                    self.attachments.iter().enumerate().map(|(index, image)| {
                        div()
                            .id(("attachment", index))
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.))
                            .bg(colors.code_background)
                            .text_size(px(12.))
                            .child(div().text_color(colors.text_muted).child("Image"))
                            .child(image.name.clone())
                            .child(
                                div()
                                    .id(("remove-attachment", index))
                                    .cursor_pointer()
                                    .text_color(colors.text_muted)
                                    .hover_style(|s| s.text_color(colors.error))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_attachment(index, cx)
                                    })),
                            )
                    }),
                ))
            })
            .child(
                text_area("composer-input")
                    .state(self.composer.downgrade())
                    .placeholder(if self.running {
                        "Steer the agent… (sent after the current step)"
                    } else {
                        "Message ZP… (/ for commands, @ to attach files)"
                    })
                    .placeholder_color(rgb_to_hsla(colors.text_muted))
                    .caret_color(rgb_to_hsla(colors.text))
                    .selection_color(rgb_to_hsla(colors.accent).opacity(0.35))
                    .caret_blink_interval_500ms()
                    .w_full()
                    .min_h(px(44.))
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .px_3()
                    .pt_2()
                    .pb_1(),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .pb_1p5()
                    .child(picker_button(Picker::Model, model_label, cx))
                    .when(model.as_ref().is_some_and(|m| m.reasoning), |this| {
                        this.child(picker_button(
                            Picker::Thinking,
                            format!("thinking {thinking}"),
                            cx,
                        ))
                    })
                    .child(picker_button(
                        Picker::Permissions,
                        self.permission_mode().label().to_owned(),
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .mr_2()
                            .text_size(px(11.))
                            .text_color(colors.text_muted)
                            .child(if self.running {
                                "Esc to stop".to_owned()
                            } else {
                                match crate::actions::shortcut_keys("zp::SendMessage", cx) {
                                    Some(keys) if keys == KEY_SEND => format!(
                                        "Shift+{} for newline",
                                        crate::ui::shortcut_label(&keys)
                                    ),
                                    Some(keys) => {
                                        format!("{} to send", crate::ui::shortcut_label(&keys))
                                    }
                                    None => String::new(),
                                }
                            }),
                    )
                    .child(action),
            );
        self.floating_composer(notes, input, cx)
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let no_keys = self.coding.is_some()
            && cx
                .try_global::<crate::store::AppModels>()
                .is_some_and(|app| app.models.available_providers().is_empty());
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .px_6()
            .child(
                div()
                    .text_size(px(22.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(zp_core::project::project_name(&self.root)),
            )
            .child(
                div()
                    .text_color(colors.text_muted)
                    .child(zp_core::project::tilde_path(&self.root)),
            )
            .when(no_keys, |this| {
                this.child(
                    div()
                        .mt_4()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .text_color(colors.text_muted)
                        .child("No model provider has an API key yet. Pick a model below and add its key, or")
                        .child(
                            div()
                                .id("empty-add-key")
                                .cursor_pointer()
                                .text_color(colors.accent)
                                .hover_style(|s| s.text_color(colors.text))
                                .child("open Settings → Providers")
                                .on_click(cx.listener(|this, _, _, cx| this.add_api_key(None, cx))),
                        ),
                )
            })
            .when_some(self.unavailable.clone(), |this, reason| {
                this.child(
                    div()
                        .mt_4()
                        .max_w(px(520.))
                        .text_color(colors.warning)
                        .child(reason),
                )
            })
            .into_any_element()
    }
}

impl ConversationView {
    fn render_tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let rows = self.tree_rows();
        let items = rows.into_iter().enumerate().map(|(index, row)| {
            let id = row.id.clone();
            div()
                .id(super::render::id("tree-node", index))
                .flex()
                .items_center()
                .gap_1p5()
                .pl(px(8. + row.depth as f32 * 14.))
                .pr_2()
                .py_0p5()
                .rounded(px(4.))
                .cursor_pointer()
                .when(!row.active, |this| this.text_color(colors.text_muted))
                .when(row.current, |this| this.bg(colors.surface_hover))
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(12.))
                        .text_color(if row.kind == super::tree::NodeKind::User {
                            colors.accent
                        } else {
                            colors.text_muted
                        })
                        .child(if row.kind == super::tree::NodeKind::User {
                            "›"
                        } else {
                            "◆"
                        }),
                )
                .child(div().flex_1().min_w_0().truncate().child(row.label))
                .when_some(row.tag, |this, tag| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded(px(3.))
                            .text_size(px(10.))
                            .bg(colors.code_background)
                            .child(tag),
                    )
                })
                .when(row.branches, |this| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(colors.text_muted)
                            .child("fork"),
                    )
                })
                .on_click(
                    cx.listener(move |this, _, window, cx| this.switch_branch(&id, window, cx)),
                )
        });
        div()
            .id("session-tree")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(340.))
            .flex()
            .flex_col()
            .bg(colors.surface)
            .border_l_1()
            .border_color(colors.border)
            .occlude()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Session tree"),
                    )
                    .child(
                        ghost_button("close-tree", "Close", cx)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_tree(cx))),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.))
                    .text_color(colors.text_muted)
                    .child("Click a message to switch to its newest branch."),
            )
            .child(
                div()
                    .id("session-tree-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1()
                    .pb_2()
                    .text_size(px(12.))
                    .children(items),
            )
            .into_any_element()
    }
}

impl Render for ConversationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transcript = if self.transcript.is_empty() {
            self.render_empty(cx)
        } else {
            list(
                self.list_state.clone(),
                cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx)),
            )
            .size_full()
            .into_any_element()
        };
        div()
            .id("conversation")
            .key_context("Conversation")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::close_picker))
            .on_action(cx.listener(Self::pick_first_model))
            .on_action(cx.listener(Self::deny_focused_approval))
            .on_action(
                cx.listener(|this, _: &crate::actions::CompletionNext, _, cx| {
                    this.move_completion(1, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CompletionPrevious, _, cx| {
                    this.move_completion(-1, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::AcceptCompletion, window, cx| {
                    this.accept_completion(None, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CloseCompletion, _, cx| {
                    this.close_completion(cx)
                }),
            )
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                    this.drop_paths(paths, window, cx)
                }),
            )
            .size_full()
            .relative()
            .child(div().size_full().pt_2().child(transcript))
            .child(self.render_composer(cx))
            .when(self.tree_open, |this| this.child(self.render_tree(cx)))
    }
}

impl ConversationView {
    /// The composer floats over the bottom of the transcript on an opaque backdrop (with
    /// a fade above it). Its height is measured so the transcript can end with a spacer
    /// of the same height: nothing is hidden behind it, and the transcript keeps its size
    /// when the composer grows. The completion menu floats above it without taking space.
    fn floating_composer(
        &self,
        notes: gpui::Div,
        input: gpui::Div,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let solid = rgb_to_hsla(colors.background);
        let clear = solid.opacity(0.);
        let height = self.composer_height.clone();
        let view = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, _, cx| {
                let measured = f32::from(bounds.size.height);
                if (height.get() - measured).abs() >= 0.5 {
                    height.set(measured);
                    cx.defer(move |cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.refresh_spacer();
                            cx.notify();
                        });
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .pb_3()
            .bg(colors.background)
            .child(
                div()
                    .absolute()
                    .top(px(-COMPOSER_FADE))
                    .left_0()
                    .right_0()
                    .h(px(COMPOSER_FADE))
                    .bg(linear_gradient(
                        180.,
                        linear_color_stop(clear, 0.),
                        linear_color_stop(solid, 1.),
                    )),
            )
            .child(measure)
            .child(centered(
                div().flex().flex_col().gap_1().child(notes).child(input),
            ))
            .when_some(self.render_completion(cx), |this, menu| {
                this.child(
                    div()
                        .absolute()
                        .bottom(relative(1.))
                        .left_0()
                        .right_0()
                        .child(centered(div().pb_1().child(menu))),
                )
            })
            .into_any_element()
    }

    /// The `/` or `@` completion menu above the composer.
    fn render_completion(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let completion = self.completion.as_ref()?;
        let colors = cx.theme().colors.clone();
        let mono = cx.theme().metrics.mono_font.clone();
        Some(
            div()
                .id("completion")
                .flex()
                .flex_col()
                .p_1()
                .rounded(px(8.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .max_h(px(280.))
                .overflow_y_scroll()
                .track_scroll(&self.completion_scroll)
                .children(completion.items.iter().enumerate().map(|(index, item)| {
                    let selected = index == completion.selected;
                    let options = completion.kind == super::composer::CompletionKind::Option;
                    div()
                        .id(("completion-item", index))
                        .flex()
                        .items_baseline()
                        .gap_3()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(selected, |this| this.bg(colors.surface_hover))
                        .hover_style(|s| s.bg(colors.surface_hover))
                        .when(options, |this| {
                            this.child(
                                div()
                                    .w(px(12.))
                                    .flex_shrink_0()
                                    .text_size(px(12.))
                                    .child(if item.current { "✓" } else { "" }),
                            )
                        })
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(colors.text_muted)
                                .child(item.detail.clone()),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.accept_completion(Some(index), window, cx);
                        }))
                }))
                .child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child(match completion.kind {
                            super::composer::CompletionKind::Command => {
                                "↑↓ to choose · Tab or Enter to complete · Esc to close"
                            }
                            super::composer::CompletionKind::File => {
                                "↑↓ to choose · Tab or Enter to attach · Esc to close"
                            }
                            super::composer::CompletionKind::Option => {
                                "Type to filter · ↑↓ to choose · Tab or Enter to select · Esc to close"
                            }
                        }),
                )
                .into_any_element(),
        )
    }
}
