//! Markdown to a small block tree that maps directly onto GPUI elements.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Inline style flags of a span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
}

/// Styled text: the plain text plus styled byte ranges and link targets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    /// Sorted, non-overlapping ranges with non-default style.
    pub spans: Vec<(Range<usize>, SpanStyle)>,
    pub links: Vec<(Range<usize>, String)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: SpanStyle) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text
            .push_str(&crate::ui::expand_tabs(text, crate::ui::TAB_WIDTH));
        let end = self.text.len();
        if style == SpanStyle::default() {
            return;
        }
        match self.spans.last_mut() {
            Some((range, last)) if range.end == start && *last == style => range.end = end,
            _ => self.spans.push((start..end, style)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// A block of a markdown document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Inline),
    Heading {
        level: u8,
        content: Inline,
    },
    Code {
        language: Option<String>,
        text: String,
    },
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Rule,
    Table {
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
}

enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Item(Vec<Block>),
}

#[derive(Clone, Copy)]
enum InlineKind {
    Paragraph,
    Heading(u8),
    Cell,
}

#[derive(Default)]
struct Table {
    header: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
    row: Vec<Inline>,
    in_head: bool,
}

#[derive(Default)]
struct StyleState {
    bold: u32,
    italic: u32,
    strike: u32,
    links: Vec<(usize, String)>,
}

impl StyleState {
    fn current(&self) -> SpanStyle {
        SpanStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code: false,
            link: !self.links.is_empty(),
        }
    }
}

struct Builder {
    frames: Vec<Frame>,
    inline: Option<(InlineKind, Inline)>,
    style: StyleState,
    code: Option<(Option<String>, String)>,
    table: Option<Table>,
}

impl Builder {
    fn push_block(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Root(blocks) | Frame::Quote(blocks) | Frame::Item(blocks)) => {
                blocks.push(block)
            }
            // Loose content directly in a list (should not happen): start an item.
            Some(Frame::List { items, .. }) => items.push(vec![block]),
            None => {}
        }
    }

    fn begin_inline(&mut self, kind: InlineKind) {
        self.end_inline();
        self.inline = Some((kind, Inline::default()));
    }

    fn end_inline(&mut self) {
        let Some((kind, inline)) = self.inline.take() else {
            return;
        };
        match kind {
            InlineKind::Cell => {
                if let Some(table) = &mut self.table {
                    table.row.push(inline);
                }
            }
            InlineKind::Heading(level) => self.push_block(Block::Heading {
                level,
                content: inline,
            }),
            InlineKind::Paragraph if inline.is_empty() => {}
            InlineKind::Paragraph => self.push_block(Block::Paragraph(inline)),
        }
    }

    /// Text outside a paragraph (tight list items) starts an implicit one.
    fn inline_mut(&mut self) -> &mut Inline {
        if self.inline.is_none() {
            self.inline = Some((InlineKind::Paragraph, Inline::default()));
        }
        &mut self.inline.as_mut().expect("just set").1
    }

    fn text(&mut self, text: &str, style: SpanStyle) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else {
            self.inline_mut().push(text, style);
        }
    }

    fn pop_frame(&mut self) -> Option<Frame> {
        self.end_inline();
        if self.frames.len() > 1 {
            self.frames.pop()
        } else {
            None
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                let style = self.style.current();
                self.text(&text, style);
            }
            Event::Code(text) => {
                let style = SpanStyle {
                    code: true,
                    ..self.style.current()
                };
                self.text(&text, style);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let style = self.style.current();
                self.text(&html, style);
            }
            Event::SoftBreak => {
                let style = self.style.current();
                self.text(" ", style);
            }
            Event::HardBreak => {
                let style = self.style.current();
                self.text("\n", style);
            }
            Event::Rule => {
                self.end_inline();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                self.text(if checked { "☑ " } else { "☐ " }, SpanStyle::default());
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.begin_inline(InlineKind::Paragraph),
            Tag::Heading { level, .. } => {
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                self.begin_inline(InlineKind::Heading(level));
            }
            Tag::BlockQuote(_) => {
                self.end_inline();
                self.frames.push(Frame::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.end_inline();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|lang| !lang.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                self.end_inline();
                self.frames.push(Frame::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.end_inline();
                self.frames.push(Frame::Item(Vec::new()));
            }
            Tag::Table(_) => {
                self.end_inline();
                self.table = Some(Table::default());
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                    table.row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row.clear();
                }
            }
            Tag::TableCell => self.begin_inline(InlineKind::Cell),
            Tag::Emphasis => self.style.italic += 1,
            Tag::Strong => self.style.bold += 1,
            Tag::Strikethrough => self.style.strike += 1,
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                let start = self.inline_mut().text.len();
                self.style.links.push((start, dest_url.into_string()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.end_inline(),
            TagEnd::BlockQuote(_) => {
                if let Some(Frame::Quote(blocks)) = self.pop_frame() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::CodeBlock => {
                if let Some((language, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    self.push_block(Block::Code { language, text });
                }
            }
            TagEnd::List(_) => {
                if let Some(Frame::List { start, items }) = self.pop_frame() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                if let Some(Frame::Item(blocks)) = self.pop_frame()
                    && let Some(Frame::List { items, .. }) = self.frames.last_mut()
                {
                    items.push(blocks);
                }
            }
            TagEnd::TableCell => self.end_inline(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header = std::mem::take(&mut table.row);
                    table.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table
                    && !table.in_head
                {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(Block::Table {
                        header: table.header,
                        rows: table.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.style.italic = self.style.italic.saturating_sub(1),
            TagEnd::Strong => self.style.bold = self.style.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.style.strike = self.style.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {
                if let Some((start, url)) = self.style.links.pop() {
                    let inline = self.inline_mut();
                    let end = inline.text.len();
                    if end > start {
                        inline.links.push((start..end, url));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Parses markdown (CommonMark plus tables, strikethrough and task lists).
pub fn parse(markdown: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder {
        frames: vec![Frame::Root(Vec::new())],
        inline: None,
        style: StyleState::default(),
        code: None,
        table: None,
    };
    for event in Parser::new_ext(markdown, options) {
        builder.handle(event);
    }
    builder.end_inline();
    // An unterminated code fence (while streaming) still shows its text.
    if let Some((language, text)) = builder.code.take() {
        builder.push_block(Block::Code { language, text });
    }
    // Close containers left open by truncated input.
    while builder.frames.len() > 1 {
        match builder.frames.pop() {
            Some(Frame::Quote(blocks)) => builder.push_block(Block::Quote(blocks)),
            Some(Frame::List { start, items }) => builder.push_block(Block::List { start, items }),
            Some(Frame::Item(blocks)) => {
                if let Some(Frame::List { items, .. }) = builder.frames.last_mut() {
                    items.push(blocks);
                }
            }
            _ => {}
        }
    }
    match builder.frames.pop() {
        Some(Frame::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(block: &Block) -> &Inline {
        match block {
            Block::Paragraph(inline) => inline,
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn inline_styles_and_links() {
        let blocks = parse("Hello **bold *both*** and `code`, ~~gone~~ [site](https://x.dev).");
        assert_eq!(blocks.len(), 1);
        let inline = paragraph(&blocks[0]);
        assert_eq!(inline.text, "Hello bold both and code, gone site.");
        let styled: Vec<(&str, SpanStyle)> = inline
            .spans
            .iter()
            .map(|(range, style)| (&inline.text[range.clone()], *style))
            .collect();
        assert_eq!(
            styled[0],
            (
                "bold ",
                SpanStyle {
                    bold: true,
                    ..Default::default()
                }
            )
        );
        assert_eq!(
            styled[1],
            (
                "both",
                SpanStyle {
                    bold: true,
                    italic: true,
                    ..Default::default()
                }
            )
        );
        assert_eq!(
            styled[2],
            (
                "code",
                SpanStyle {
                    code: true,
                    ..Default::default()
                }
            )
        );
        assert_eq!(
            styled[3],
            (
                "gone",
                SpanStyle {
                    strike: true,
                    ..Default::default()
                }
            )
        );
        assert_eq!(
            styled[4],
            (
                "site",
                SpanStyle {
                    link: true,
                    ..Default::default()
                }
            )
        );
        assert_eq!(inline.links, [(31..35, "https://x.dev".to_owned())]);
    }

    #[test]
    fn blocks_and_nesting() {
        let blocks = parse(
            "# Title\n\nText\nsoft break\n\n```rust\nfn main() {}\n```\n\n- one\n- two\n  - nested\n\n3. three\n4. four\n\n> quoted\n\n---\n\n- [x] done\n",
        );
        assert!(
            matches!(&blocks[0], Block::Heading { level: 1, content } if content.text == "Title")
        );
        assert_eq!(paragraph(&blocks[1]).text, "Text soft break");
        assert_eq!(
            blocks[2],
            Block::Code {
                language: Some("rust".into()),
                text: "fn main() {}".into()
            }
        );
        let Block::List { start: None, items } = &blocks[3] else {
            panic!("{:?}", blocks[3])
        };
        assert_eq!(items.len(), 2);
        assert_eq!(paragraph(&items[0][0]).text, "one");
        assert!(matches!(&items[1][1], Block::List { items, .. } if items.len() == 1));
        assert!(matches!(&blocks[4], Block::List { start: Some(3), items } if items.len() == 2));
        assert!(matches!(&blocks[5], Block::Quote(inner) if paragraph(&inner[0]).text == "quoted"));
        assert_eq!(blocks[6], Block::Rule);
        let Block::List { items, .. } = &blocks[7] else {
            panic!()
        };
        assert_eq!(paragraph(&items[0][0]).text, "☑ done");
    }

    #[test]
    fn tables() {
        let blocks = parse("| a | b |\n|---|---|\n| 1 | **2** |\n| 3 | 4 |\n");
        let Block::Table { header, rows } = &blocks[0] else {
            panic!("{blocks:?}")
        };
        let texts = |cells: &[Inline]| cells.iter().map(|c| c.text.clone()).collect::<Vec<_>>();
        assert_eq!(texts(header), ["a", "b"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(texts(&rows[0]), ["1", "2"]);
        assert!(rows[0][1].spans[0].1.bold);
    }

    #[test]
    fn partial_input_while_streaming() {
        let blocks = parse("Here:\n\n```py\nprint(1)\nprint(");
        assert_eq!(
            blocks[1],
            Block::Code {
                language: Some("py".into()),
                text: "print(1)\nprint(".into()
            }
        );
        let blocks = parse("- a\n- b **unfinished");
        let Block::List { items, .. } = &blocks[0] else {
            panic!()
        };
        assert_eq!(paragraph(&items[1][0]).text, "b **unfinished");
        assert!(parse("").is_empty());
    }
}
