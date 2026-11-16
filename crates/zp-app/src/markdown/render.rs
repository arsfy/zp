//! Renders parsed markdown blocks as GPUI elements.

use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FontStyle, FontWeight, HighlightStyle, IntoElement,
    Rgba, SharedString, StrikethroughStyle, UnderlineStyle, div, prelude::*, px, rgb_to_hsla,
};

use super::parse::{Block, Inline};
use crate::selectable_text::{SelectableText, TextSelection};
use crate::theme::ActiveTheme;

/// Colors and fonts used for markdown.
#[derive(Clone)]
pub struct MarkdownStyle {
    pub text: Rgba,
    pub muted: Rgba,
    pub link: Rgba,
    pub border: Rgba,
    pub code_background: Rgba,
    pub mono_font: SharedString,
}

impl MarkdownStyle {
    pub fn from_theme(cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            text: theme.colors.text,
            muted: theme.colors.text_muted,
            link: theme.colors.accent,
            border: theme.colors.border,
            code_background: theme.colors.code_background,
            mono_font: theme.metrics.mono_font.clone(),
        }
    }
}

/// Builds unique element ids within one rendered document.
pub struct IdGen {
    base: SharedString,
    next: usize,
    document: (usize, usize),
    selection: TextSelection,
    separator: &'static str,
}

impl IdGen {
    pub fn new(
        base: impl Into<SharedString>,
        document: (usize, usize),
        selection: TextSelection,
    ) -> Self {
        selection.begin_document(document.0, document.1);
        Self {
            base: base.into(),
            next: 0,
            document,
            selection,
            separator: "\n\n",
        }
    }

    fn next(&mut self) -> ElementId {
        self.next += 1;
        ElementId::NamedInteger(self.base.clone(), self.next as u64)
    }

    fn text(&mut self, content: SharedString) -> SelectableText {
        let element_id = self.next();
        SelectableText::new(
            element_id,
            (self.document.0, self.document.1, self.next),
            content,
            self.selection.clone(),
            std::mem::replace(&mut self.separator, ""),
        )
    }
}

/// Renders `blocks` as a column.
pub fn render_markdown(blocks: &[Block], style: &MarkdownStyle, ids: &mut IdGen) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .children(blocks.iter().map(|block| render_block(block, style, ids)))
        .into_any_element()
}

fn render_inline(inline: &Inline, style: &MarkdownStyle, ids: &mut IdGen) -> AnyElement {
    let code_ranges: Vec<_> = inline
        .spans
        .iter()
        .filter(|(_, span)| span.code)
        .map(|(range, _)| range.clone())
        .collect();
    let highlights: Vec<_> = inline
        .spans
        .iter()
        .map(|(range, span)| {
            let mut highlight = HighlightStyle::default();
            if span.bold {
                highlight.font_weight = Some(FontWeight::BOLD);
            }
            if span.italic {
                highlight.font_style = Some(FontStyle::Italic);
            }
            if span.strike {
                highlight.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: None,
                });
            }
            if span.link {
                highlight.color = Some(rgb_to_hsla(style.link));
                highlight.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: None,
                    wavy: false,
                });
            }
            (range.clone(), highlight)
        })
        .collect();
    ids.text(inline.text.clone().into())
        .highlights(highlights)
        .inline_code(code_ranges, style.mono_font.clone(), style.code_background)
        .links(inline.links.clone())
        .into_any_element()
}

fn render_block(block: &Block, style: &MarkdownStyle, ids: &mut IdGen) -> AnyElement {
    ids.separator = "\n\n";
    match block {
        Block::Paragraph(inline) => div()
            .child(render_inline(inline, style, ids))
            .into_any_element(),
        Block::Heading { level, content } => {
            let size = match level {
                1 => 20.,
                2 => 17.,
                3 => 15.,
                _ => 13.,
            };
            div()
                .pt_1()
                .text_size(px(size))
                .font_weight(FontWeight::SEMIBOLD)
                .child(render_inline(content, style, ids))
                .into_any_element()
        }
        Block::Code { language, text } => code_block(language.as_deref(), text, style, ids),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap_1()
            .children(items.iter().enumerate().map(|(index, item)| {
                let marker = match start {
                    Some(start) => format!("{}.", start + index as u64),
                    None => "•".to_owned(),
                };
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .min_w(px(16.))
                            .text_color(style.muted)
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(render_markdown(item, style, ids)),
                    )
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .pl_3()
            .border_l_2()
            .border_color(style.border)
            .text_color(style.muted)
            .child(render_markdown(blocks, style, ids))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).my_1().bg(style.border).into_any_element(),
        Block::Table { header, rows } => {
            let cell = |inline: &Inline, bold: bool, ids: &mut IdGen| {
                let content = render_inline(inline, style, ids);
                ids.separator = "\t";
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .when(bold, |this| this.font_weight(FontWeight::SEMIBOLD))
                    .child(content)
            };
            let mut table = div()
                .flex()
                .flex_col()
                .border_1()
                .border_color(style.border)
                .rounded(px(4.));
            table = table.child(
                div().flex().flex_row().bg(style.code_background).children(
                    header
                        .iter()
                        .map(|c| cell(c, true, ids))
                        .collect::<Vec<_>>(),
                ),
            );
            for row in rows {
                ids.separator = "\n";
                table = table.child(
                    div()
                        .flex()
                        .flex_row()
                        .border_t_1()
                        .border_color(style.border)
                        .children(row.iter().map(|c| cell(c, false, ids)).collect::<Vec<_>>()),
                );
            }
            table.into_any_element()
        }
    }
}

/// A code block with its language and a copy button.
fn code_block(
    language: Option<&str>,
    text: &str,
    style: &MarkdownStyle,
    ids: &mut IdGen,
) -> AnyElement {
    let copied = text.to_owned();
    div()
        .flex()
        .flex_col()
        .rounded(px(6.))
        .bg(style.code_background)
        .border_1()
        .border_color(style.border)
        .child(
            div()
                .flex()
                .justify_between()
                .px_2()
                .pt_1()
                .text_size(px(11.))
                .text_color(style.muted)
                .child(language.unwrap_or_default().to_owned())
                .child(
                    div()
                        .id(ids.next())
                        .cursor_pointer()
                        .hover_style(|s| s.text_color(style.text))
                        .child("Copy")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()))
                        }),
                ),
        )
        .child(
            div()
                .px_3()
                .pb_2()
                .pt_1()
                .font_family(style.mono_font.clone())
                .text_size(px(12.))
                .child(ids.text(text.to_owned().into())),
        )
        .into_any_element()
}
