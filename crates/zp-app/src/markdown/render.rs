//! Renders parsed markdown blocks as GPUI elements.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FontStyle, FontWeight, HighlightStyle,
    InteractiveText, IntoElement, Rgba, SharedString, StrikethroughStyle, StyledText,
    UnderlineStyle, div, prelude::*, px, rgb_to_hsla,
};

use super::parse::{Block, Inline};
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
}

impl IdGen {
    pub fn new(base: impl Into<SharedString>) -> Self {
        Self {
            base: base.into(),
            next: 0,
        }
    }

    fn next(&mut self) -> ElementId {
        self.next += 1;
        ElementId::NamedInteger(self.base.clone(), self.next as u64)
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
    if code_ranges.is_empty() {
        return render_inline_text(inline, style, ids);
    }

    // Inline boxes reserve space for padding in the surrounding text flow.
    let mut paragraph = div();
    let mut offset = 0;
    for range in code_ranges {
        if offset < range.start {
            paragraph = paragraph.child(render_inline_text(
                &inline_slice(inline, offset..range.start),
                style,
                ids,
            ));
        }
        let code = inline_slice(inline, range.clone());
        paragraph = paragraph.child(
            div()
                .inline_flex()
                .align_middle()
                .px_1()
                .py(px(1.))
                .rounded(px(4.))
                .bg(style.code_background)
                .child(render_inline_text(&code, style, ids)),
        );
        offset = range.end;
    }
    if offset < inline.text.len() {
        paragraph = paragraph.child(render_inline_text(
            &inline_slice(inline, offset..inline.text.len()),
            style,
            ids,
        ));
    }
    paragraph.into_any_element()
}

fn inline_slice(inline: &Inline, range: Range<usize>) -> Inline {
    let intersect = |other: &Range<usize>| {
        let start = other.start.max(range.start);
        let end = other.end.min(range.end);
        (start < end).then(|| start - range.start..end - range.start)
    };
    Inline {
        text: inline.text[range.clone()].to_owned(),
        spans: inline
            .spans
            .iter()
            .filter_map(|(range, span)| intersect(range).map(|range| (range, *span)))
            .collect(),
        links: inline
            .links
            .iter()
            .filter_map(|(range, url)| intersect(range).map(|range| (range, url.clone())))
            .collect(),
    }
}

fn render_inline_text(inline: &Inline, style: &MarkdownStyle, ids: &mut IdGen) -> AnyElement {
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
    let code_fonts: Vec<_> = inline
        .spans
        .iter()
        .filter(|(_, span)| span.code)
        .map(|(range, _)| (range.clone(), style.mono_font.clone()))
        .collect();
    let text = StyledText::new(inline.text.clone())
        .with_highlights(highlights)
        .with_font_family_overrides(code_fonts);
    if inline.links.is_empty() {
        return text.into_any_element();
    }
    let urls: Rc<Vec<String>> = Rc::new(inline.links.iter().map(|(_, url)| url.clone()).collect());
    let ranges = inline
        .links
        .iter()
        .map(|(range, _)| range.clone())
        .collect();
    InteractiveText::new(ids.next(), text)
        .on_click(ranges, move |index, _, cx| {
            if let Some(url) = urls.get(index) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

fn render_block(block: &Block, style: &MarkdownStyle, ids: &mut IdGen) -> AnyElement {
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
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .when(bold, |this| this.font_weight(FontWeight::SEMIBOLD))
                    .child(render_inline(inline, style, ids))
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
                .child(text.to_owned()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_code_segments_preserve_surrounding_styles_and_links() {
        let blocks = super::super::parse::parse(
            "**Before [`cargo test` and docs](https://example.com) after**",
        );
        let Block::Paragraph(inline) = &blocks[0] else {
            panic!("expected a paragraph");
        };
        let code = inline
            .spans
            .iter()
            .find(|(_, span)| span.code)
            .unwrap()
            .0
            .clone();
        let before = inline_slice(inline, 0..code.start);
        let command = inline_slice(inline, code.clone());
        let after = inline_slice(inline, code.end..inline.text.len());

        assert_eq!(before.text, "Before ");
        assert!(before.links.is_empty());
        assert_eq!(command.text, "cargo test");
        assert!(command.spans[0].1.bold && command.spans[0].1.code);
        assert_eq!(command.links, vec![(0..10, "https://example.com".into())]);
        assert_eq!(after.text, " and docs after");
        assert_eq!(after.links, vec![(0..9, "https://example.com".into())]);
        assert!(after.spans.iter().all(|(_, span)| span.bold));
    }
}
