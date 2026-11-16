//! Rendering of the conversation: transcript rows, tool cards and the composer.

use gpui::{
    Anchor, AnyElement, App, ClipboardItem, ColorExt as _, Context, ElementId, FontWeight,
    IntoElement, Render, SharedString, Window, anchored, deferred, div, list, point, prelude::*,
    px, rgb_to_hsla,
};
use gpui_elements::editable_text::{text_area, text_input};
use zp_ai::{AssistantContent, StopReason, ToolCall};
use zp_core::checkpoint::RecordedChange;

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

fn ghost_button(
    element_id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = &cx.theme().colors;
    div()
        .id(element_id)
        .px_2()
        .py_0p5()
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(colors.text_muted)
        .cursor_pointer()
        .hover_style(|s| s.bg(colors.surface_hover).text_color(colors.text))
        .child(label.into())
}

impl ConversationView {
    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = self.transcript.rows().get(ix).cloned();
        let content = match &row {
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
        let group = SharedString::from(format!("row-{ix}"));
        let actions = row.and_then(|row| self.render_row_actions(ix, &row, group.clone(), cx));
        centered(
            div()
                .group(group)
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .child(content)
                .children(actions),
        )
        .into_any_element()
    }

    /// Message actions, shown when the row is hovered (not while running).
    fn render_row_actions(
        &self,
        ix: usize,
        row: &Row,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.running || self.coding.is_none() {
            return None;
        }
        let (text, is_user) = match row {
            Row::User { text, .. } => (text.clone(), true),
            Row::Assistant { .. } => {
                let message = self.transcript.assistant(ix)?;
                if self.transcript.is_streaming(ix) {
                    return None;
                }
                (message.text(), false)
            }
            Row::Notice { .. } => return None,
        };
        let mut bar = div()
            .flex()
            .gap_1()
            .invisible()
            .group_hover_style(group, |style| style.visible());
        if is_user {
            bar = bar.child(ghost_button(id("edit", ix), "Edit & Resend", cx).on_click(
                cx.listener(move |this, _, window, cx| this.edit_and_resend(ix, window, cx)),
            ));
        } else {
            bar = bar.child(
                ghost_button(id("branch", ix), "Branch from here", cx).on_click(
                    cx.listener(move |this, _, window, cx| this.branch_from(ix, window, cx)),
                ),
            );
        }
        bar = bar.child(
            ghost_button(id("fork", ix), "Fork to new tab", cx)
                .on_click(cx.listener(move |this, _, _, cx| this.fork_from(ix, cx))),
        );
        if !text.trim().is_empty() {
            bar = bar.child(
                ghost_button(id("copy", ix), "Copy", cx).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                }),
            );
        }
        Some(bar.into_any_element())
    }

    fn render_user(&self, text: &str, images: usize, files: &[String], cx: &App) -> AnyElement {
        let colors = &cx.theme().colors;
        div()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(colors.surface)
            .border_1()
            .border_color(colors.border)
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
            })
            .into_any_element()
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
        if streaming && message.content.is_empty() {
            column = column.child(div().text_color(colors.text_muted).child("Working…"));
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
        let current = self
            .coding
            .as_ref()
            .map(|c| c.agent().model().qualified_id());
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
                this.child(div().px_2().py_1().text_color(colors.text_muted).child(
                    if self.model_choices(cx).is_empty() {
                        "No provider has credentials. Set an API key and restart."
                    } else {
                        "No matching models"
                    },
                ))
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
        let Some(coding) = &self.coding else {
            return div().into_any_element();
        };
        let model = coding.agent().model();
        let current = model.clamp_thinking_level(coding.agent().thinking_level());
        let items = model
            .supported_thinking_levels()
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
        let model = coding.agent().model();
        let thinking = model.clamp_thinking_level(coding.agent().thinking_level());
        let queued = coding.agent().steering_queue().len();

        let picker_button = |picker: Picker, label: String, cx: &mut Context<Self>| {
            let open = self.picker == Some(picker);
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
                    ghost_button(
                        match picker {
                            Picker::Model => "model-button",
                            Picker::Thinking => "thinking-button",
                            Picker::Permissions => "permissions-button",
                        },
                        format!("{label} ▾"),
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

        centered(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .pb_3()
                .when_some(self.run_error.clone(), |this, error| {
                    this.child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.error)
                            .child(error),
                    )
                })
                .when_some(self.render_completion(cx), |this, menu| this.child(menu))
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
                .child(
                    div()
                        .key_context(if self.completion.is_some() {
                            "Composer CompletionMenu"
                        } else {
                            "Composer"
                        })
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
                                .child(picker_button(Picker::Model, model.qualified_id(), cx))
                                .when(model.reasoning, |this| {
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
                                        .text_size(px(11.))
                                        .text_color(colors.text_muted)
                                        .child(if self.running {
                                            "Esc to stop".to_owned()
                                        } else {
                                            match crate::actions::shortcut_keys(
                                                "zp::SendMessage",
                                                cx,
                                            ) {
                                                Some(keys) if keys == KEY_SEND => format!(
                                                    "{} to send · Shift+{} for newline",
                                                    crate::ui::shortcut_label(&keys),
                                                    crate::ui::shortcut_label(&keys)
                                                ),
                                                Some(keys) => format!(
                                                    "{} to send",
                                                    crate::ui::shortcut_label(&keys)
                                                ),
                                                None => String::new(),
                                            }
                                        }),
                                )
                                .child(action),
                        ),
                ),
        )
        .into_any_element()
    }

    fn render_empty(&self, cx: &App) -> AnyElement {
        let colors = &cx.theme().colors;
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
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().pt_2().child(transcript))
            .child(self.render_composer(cx))
            .when(self.tree_open, |this| this.child(self.render_tree(cx)))
    }
}

impl ConversationView {
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
                .children(completion.items.iter().enumerate().map(|(index, item)| {
                    let selected = index == completion.selected;
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
                        }),
                )
                .into_any_element(),
        )
    }
}
