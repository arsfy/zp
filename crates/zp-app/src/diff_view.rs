//! Diffs of files: the full-size diff view (unified or side by side) and the line
//! renderers shared with the inline diffs of tool cards.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ClipboardItem, ColorExt as _, Context, DispatchPhase, ElementId, EventEmitter,
    FocusHandle, Focusable, FontWeight, HighlightStyle, IntoElement, ListHorizontalSizingBehavior,
    MouseButton, Pixels, Render, Rgba, SharedString, StyledText, UniformListScrollHandle, Window,
    div, prelude::*, px, uniform_list,
};
use zp_core::changes::RevertStatus;
use zp_core::diff::{DEFAULT_CONTEXT, DiffLine, FileDiff, LineKind, SideLine, SplitRow};

use crate::actions::{CloseDiff, NextDiffFile, PreviousDiffFile, ToggleDiffLayout};
use crate::markdown::highlight::{Highlights, MAX_HIGHLIGHTED_BYTES, highlight};
use crate::selectable_text::{SelectableText, TextId, TextSelection};
use crate::theme::ActiveTheme;

/// Height of one diff row.
const ROW_HEIGHT: f32 = 20.;
/// Font size of diff text.
const FONT_SIZE: f32 = 12.;
/// Width of the `+` / `-` column.
const SIGN_WIDTH: f32 = 16.;
/// Space after the text of a line (horizontal scrolling ends there).
const TEXT_END_PADDING: f32 = 16.;
/// Digit width used when the font cannot be measured (inline previews).
const ESTIMATED_DIGIT_WIDTH: f32 = 7.5;

/// Something that reverts the change shown (a file of a session or turn).
pub type RevertFn = Rc<dyn Fn(&mut Window, &mut App)>;

/// One file in a diff view.
#[derive(Clone)]
pub struct DiffItem {
    pub path: PathBuf,
    /// The path as shown (relative to the project).
    pub display: String,
    /// What is compared, e.g. "Session changes" or "Working tree vs HEAD".
    pub caption: String,
    /// `None`: the file did not exist.
    pub before: Option<Arc<str>>,
    /// `None`: the file was deleted.
    pub after: Option<Arc<str>>,
    /// A version was too large to read, so `before` and `after` were not loaded and the
    /// diff is only shown as too large.
    pub too_large: bool,
    pub status: RevertStatus,
    /// Offered as "Revert" when set.
    pub revert: Option<RevertFn>,
}

impl DiffItem {
    pub fn diff(&self) -> FileDiff {
        if self.too_large {
            return FileDiff {
                too_large: true,
                ..FileDiff::default()
            };
        }
        FileDiff::compute(
            self.before.as_deref(),
            self.after.as_deref(),
            DEFAULT_CONTEXT,
        )
    }
}

/// Unified or side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLayout {
    Unified,
    Split,
}

/// A row of the unified layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifiedRow {
    Header(String),
    Line(DiffLine),
}

/// The unified rows of a diff.
pub fn unified_rows(diff: &FileDiff) -> Vec<UnifiedRow> {
    let mut rows = Vec::with_capacity(diff.line_count() + diff.hunks.len());
    for hunk in &diff.hunks {
        rows.push(UnifiedRow::Header(hunk.header()));
        rows.extend(hunk.lines.iter().cloned().map(UnifiedRow::Line));
    }
    rows
}

/// Colors of diff lines.
#[derive(Clone)]
pub struct DiffColors {
    pub text: Rgba,
    pub muted: Rgba,
    pub added: Rgba,
    pub removed: Rgba,
    pub added_bg: Rgba,
    pub removed_bg: Rgba,
    pub header_bg: Rgba,
    pub mono: SharedString,
}

impl DiffColors {
    pub fn of(cx: &App) -> Self {
        let theme = cx.theme();
        let colors = &theme.colors;
        Self {
            text: colors.text,
            muted: colors.text_muted,
            added: colors.success,
            removed: colors.error,
            added_bg: colors.success.opacity(0.14),
            removed_bg: colors.error.opacity(0.14),
            header_bg: colors.accent.opacity(0.08),
            mono: theme.metrics.mono_font.clone(),
        }
    }

    fn line_bg(&self, kind: LineKind) -> Option<Rgba> {
        match kind {
            LineKind::Added => Some(self.added_bg),
            LineKind::Removed => Some(self.removed_bg),
            LineKind::Context => None,
        }
    }
}

/// Widths of the line number columns, from the largest number shown; `None` hides a
/// column that would stay empty (old numbers of a new file, new ones of a deleted file).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gutters {
    pub old: Option<Pixels>,
    pub new: Option<Pixels>,
}

impl Gutters {
    /// Gutters for `diff` with digits `digit` wide.
    pub fn of(diff: &FileDiff, digit: Pixels) -> Self {
        let lines = || diff.hunks.iter().flat_map(|hunk| hunk.lines.iter());
        let width = |n: usize| digit * n.to_string().len() as f32 + px(16.);
        Self {
            old: lines().filter_map(|line| line.old).max().map(width),
            new: lines().filter_map(|line| line.new).max().map(width),
        }
    }

    /// Gutters with an estimated digit width (where no window is at hand).
    pub fn estimate(diff: &FileDiff) -> Self {
        Self::of(diff, px(ESTIMATED_DIGIT_WIDTH))
    }

    fn total(&self) -> Pixels {
        self.old.unwrap_or_default() + self.new.unwrap_or_default()
    }
}

/// Text as drawn: tabs as four spaces (a single-line text run would collapse them).
fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

/// One version of a diffed file, indexed in the same line order as the diff.
struct DiffSource {
    text: Arc<str>,
    starts: Rc<[usize]>,
}

impl DiffSource {
    fn new(text: &Arc<str>) -> Option<Self> {
        if text.len() > MAX_HIGHLIGHTED_BYTES {
            return None;
        }
        let text = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n").into()
        } else {
            text.clone()
        };
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        Some(Self { text, starts })
    }

    fn highlight(&self, path: &str, dark: bool) -> Option<DiffHighlights> {
        Some(DiffHighlights {
            spans: highlight(Some(path), &self.text, dark)?,
            starts: self.starts.clone(),
        })
    }
}

struct DiffHighlights {
    spans: Highlights,
    starts: Rc<[usize]>,
}

impl DiffHighlights {
    fn line(&self, number: usize, text: &str) -> Vec<(Range<usize>, HighlightStyle)> {
        let Some(&start) = number
            .checked_sub(1)
            .and_then(|index| self.starts.get(index))
        else {
            return Vec::new();
        };
        let end = start + text.len();
        let first = self.spans.partition_point(|(range, _)| range.end <= start);
        let tabs: Vec<_> = text.match_indices('\t').map(|(offset, _)| offset).collect();
        // A tab occupies one byte in the source and four spaces in the diff.
        let display_offset = |offset| offset + 3 * tabs.partition_point(|&tab| tab < offset);
        self.spans[first..]
            .iter()
            .take_while(|(range, _)| range.start < end)
            .filter_map(|(range, style)| {
                let from = range.start.saturating_sub(start);
                let to = range.end.min(end) - start;
                (from < to).then(|| (display_offset(from)..display_offset(to), *style))
            })
            .collect()
    }
}

/// Columns `text` takes in a monospace font: wide (CJK, fullwidth, emoji) characters
/// take two.
fn display_columns(text: &str) -> usize {
    text.chars()
        .map(|c| match c as u32 {
            0x09 => 4,
            0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD => 2,
            _ => 1,
        })
        .sum()
}

fn gutter(number: Option<usize>, width: Pixels, colors: &DiffColors) -> impl IntoElement {
    div()
        .w(width)
        .flex_shrink_0()
        .pr_2()
        .flex()
        .justify_end()
        .text_color(colors.muted)
        .child(number.map(|n| n.to_string()).unwrap_or_default())
}

/// A line's text: selectable and as wide as it is (the diff view scrolls sideways), or
/// cut off at the row's end (small previews).
fn code(
    text: &str,
    selection: Option<(&TextSelection, TextId)>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
) -> AnyElement {
    let text: SharedString = expand_tabs(text).into();
    match selection {
        Some((selection, id)) => div()
            .flex_shrink_0()
            .pr(px(TEXT_END_PADDING))
            .child(
                SelectableText::new(
                    ElementId::Name(format!("diff-text-{}-{}", id.0, id.1).into()),
                    id,
                    text,
                    selection.clone(),
                    "\n",
                )
                .highlights(highlights),
            )
            .into_any_element(),
        None => div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(StyledText::new(text).with_highlights(highlights))
            .into_any_element(),
    }
}

fn row_base(selectable: bool, colors: &DiffColors) -> gpui::Div {
    div()
        .h(px(ROW_HEIGHT))
        .flex()
        .items_center()
        .whitespace_nowrap()
        .font_family(colors.mono.clone())
        .text_size(px(FONT_SIZE))
        .when(selectable, |row| row.min_w_full())
        .when(!selectable, |row| row.w_full())
}

/// A unified diff row; with `selection` (and the row's index) its text can be selected.
pub fn render_unified_row(
    row: &UnifiedRow,
    colors: &DiffColors,
    gutters: Gutters,
    selection: Option<(&TextSelection, usize)>,
) -> AnyElement {
    render_unified(row, colors, gutters, selection, [&None, &None])
}

fn render_unified(
    row: &UnifiedRow,
    colors: &DiffColors,
    gutters: Gutters,
    selection: Option<(&TextSelection, usize)>,
    [old, new]: [&Option<DiffHighlights>; 2],
) -> AnyElement {
    let base = row_base(selection.is_some(), colors);
    match row {
        UnifiedRow::Header(header) => base
            .bg(colors.header_bg)
            .text_color(colors.muted)
            .pl(gutters.total() + px(SIGN_WIDTH))
            .child(header.clone())
            .into_any_element(),
        UnifiedRow::Line(line) => {
            let syntax = match line.new {
                Some(number) => new.as_ref().map(|syntax| (syntax, number)),
                None => old.as_ref().zip(line.old),
            };
            let highlights =
                syntax.map_or_else(Vec::new, |(syntax, number)| syntax.line(number, &line.text));
            let (sign, color) = match line.kind {
                LineKind::Added => ("+", colors.added),
                LineKind::Removed => ("-", colors.removed),
                LineKind::Context => (" ", colors.muted),
            };
            base.when_some(colors.line_bg(line.kind), |this, bg| this.bg(bg))
                .text_color(colors.text)
                .when_some(gutters.old, |this, width| {
                    this.child(gutter(line.old, width, colors))
                })
                .when_some(gutters.new, |this, width| {
                    this.child(gutter(line.new, width, colors))
                })
                .child(
                    div()
                        .w(px(SIGN_WIDTH))
                        .flex_shrink_0()
                        .text_color(color)
                        .child(sign),
                )
                .child(code(
                    &line.text,
                    selection.map(|(selection, index)| (selection, (0, index, 0))),
                    highlights,
                ))
                .into_any_element()
        }
    }
}

/// One side of a split row: at least `width` wide, so long lines scroll sideways.
fn render_side(
    side: Option<&SideLine>,
    column: usize,
    index: usize,
    (gutter_width, width): (Pixels, Pixels),
    colors: &DiffColors,
    selection: &TextSelection,
    syntax: Option<&DiffHighlights>,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(width)
        .h_full()
        .flex()
        .items_center()
        .overflow_hidden()
        .when_some(side.and_then(|s| colors.line_bg(s.kind)), |this, bg| {
            this.bg(bg)
        })
        .when(side.is_none(), |this| this.bg(colors.muted.opacity(0.05)))
        .when_some(side, |this, side| {
            this.child(gutter(Some(side.number), gutter_width, colors))
                .child(code(
                    &side.text,
                    Some((selection, (column, index, 0))),
                    syntax.map_or_else(Vec::new, |syntax| syntax.line(side.number, &side.text)),
                ))
        })
}

/// Widths of the split layout: each side's line number column and its minimum width.
#[derive(Clone, Copy, Debug)]
pub struct SplitWidths {
    gutters: (Pixels, Pixels),
    sides: (Pixels, Pixels),
}

impl SplitWidths {
    fn of(rows: &[SplitRow], gutters: Gutters, digit: Pixels) -> Self {
        let columns = |side: fn(&SplitRow) -> Option<&SideLine>| {
            rows.iter()
                .filter_map(side)
                .map(|line| display_columns(&line.text))
                .max()
                .unwrap_or(0)
        };
        let left = columns(|row| match row {
            SplitRow::Lines { left, .. } => left.as_ref(),
            SplitRow::Header(_) => None,
        });
        let right = columns(|row| match row {
            SplitRow::Lines { right, .. } => right.as_ref(),
            SplitRow::Header(_) => None,
        });
        let gutters = (
            gutters.old.unwrap_or(digit + px(16.)),
            gutters.new.unwrap_or(digit + px(16.)),
        );
        let side =
            |gutter: Pixels, columns: usize| gutter + digit * columns as f32 + px(TEXT_END_PADDING);
        Self {
            gutters,
            sides: (side(gutters.0, left), side(gutters.1, right)),
        }
    }
}

/// A side-by-side diff row; its text can be selected, one side at a time.
fn render_split_row(
    row: &SplitRow,
    index: usize,
    diff: &FileDiff,
    colors: &DiffColors,
    widths: SplitWidths,
    selection: &TextSelection,
    [old, new]: [&Option<DiffHighlights>; 2],
) -> AnyElement {
    let base = row_base(true, colors).text_color(colors.text);
    match row {
        SplitRow::Header(hunk) => base
            .bg(colors.header_bg)
            .text_color(colors.muted)
            .pl(widths.gutters.0)
            .child(diff.hunks[*hunk].header())
            .into_any_element(),
        SplitRow::Lines { left, right } => base
            .child(render_side(
                left.as_ref(),
                0,
                index,
                (widths.gutters.0, widths.sides.0),
                colors,
                selection,
                old.as_ref(),
            ))
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .flex_shrink_0()
                    .bg(colors.muted.opacity(0.3)),
            )
            .child(render_side(
                right.as_ref(),
                1,
                index,
                (widths.gutters.1, widths.sides.1),
                colors,
                selection,
                new.as_ref(),
            ))
            .into_any_element(),
    }
}

/// Width of a digit in the monospace diff font.
fn digit_width(font: &SharedString, window: &Window) -> Pixels {
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&gpui::font(font.clone()));
    text_system
        .advance(font_id, px(FONT_SIZE), '0')
        .map(|size| size.width)
        .unwrap_or(px(ESTIMATED_DIGIT_WIDTH))
}

/// Events of a [`DiffView`].
pub enum DiffViewEvent {
    Close,
}

/// A full-size diff of one file, with navigation between the files it was opened with.
pub struct DiffView {
    items: Vec<DiffItem>,
    index: usize,
    layout: DiffLayout,
    diff: Rc<FileDiff>,
    unified: Rc<Vec<UnifiedRow>>,
    split: Rc<Vec<SplitRow>>,
    sources: [Option<DiffSource>; 2],
    scroll: UniformListScrollHandle,
    /// Selected text across the lines, for copying.
    selection: TextSelection,
    focus_handle: FocusHandle,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    pub fn new(
        items: Vec<DiffItem>,
        index: usize,
        layout: DiffLayout,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            items,
            index: 0,
            layout,
            diff: Rc::default(),
            unified: Rc::default(),
            split: Rc::default(),
            sources: [None, None],
            scroll: UniformListScrollHandle::new(),
            selection: TextSelection::new(focus_handle.clone()),
            focus_handle,
        };
        this.show(index);
        this
    }

    fn show(&mut self, index: usize) {
        self.index = index.min(self.items.len().saturating_sub(1));
        let diff = self
            .items
            .get(self.index)
            .map(DiffItem::diff)
            .unwrap_or_default();
        self.unified = Rc::new(unified_rows(&diff));
        self.split = Rc::new(diff.split_rows());
        self.sources = match self.items.get(self.index) {
            Some(item) if !diff.too_large && !diff.is_empty() => [
                item.before.as_ref().and_then(DiffSource::new),
                item.after.as_ref().and_then(DiffSource::new),
            ],
            _ => [None, None],
        };
        self.diff = Rc::new(diff);
        self.scroll = UniformListScrollHandle::new();
        self.reset_selection();
    }

    /// Makes every line of the current layout selectable (also lines not drawn yet).
    fn reset_selection(&self) {
        match self.layout {
            DiffLayout::Unified => self.selection.set_fragments(
                self.unified
                    .iter()
                    .enumerate()
                    .filter_map(|(index, row)| match row {
                        UnifiedRow::Line(line) => {
                            Some(((0, index, 0), expand_tabs(&line.text).into()))
                        }
                        UnifiedRow::Header(_) => None,
                    }),
                "\n",
            ),
            DiffLayout::Split => self.selection.set_fragments(
                self.split
                    .iter()
                    .enumerate()
                    .flat_map(|(index, row)| match row {
                        SplitRow::Lines { left, right } => [
                            left.as_ref()
                                .map(|l| ((0, index, 0), expand_tabs(&l.text).into())),
                            right
                                .as_ref()
                                .map(|r| ((1, index, 0), expand_tabs(&r.text).into())),
                        ],
                        SplitRow::Header(_) => [None, None],
                    })
                    .flatten(),
                "\n",
            ),
        }
    }

    /// The text selection (tests).
    #[cfg(test)]
    pub fn selection(&self) -> TextSelection {
        self.selection.clone()
    }

    /// The horizontal scroll offset, the list's width and the width of its content
    /// (tests).
    #[cfg(test)]
    pub fn horizontal_scroll(&self) -> (Pixels, Pixels, Pixels) {
        let state = self.scroll.0.borrow();
        let size = state.last_item_size.expect("laid out");
        (
            state.base_handle.offset().x,
            size.item.width,
            size.contents.width,
        )
    }

    /// The row the list measures for its width: the widest one.
    fn widest_row(&self) -> usize {
        match self.layout {
            DiffLayout::Unified => self
                .unified
                .iter()
                .enumerate()
                .max_by_key(|(_, row)| match row {
                    UnifiedRow::Line(line) => display_columns(&line.text),
                    UnifiedRow::Header(header) => header.len(),
                })
                .map_or(0, |(index, _)| index),
            // Every line row has both sides at their minimum widths.
            DiffLayout::Split => self
                .split
                .iter()
                .position(|row| matches!(row, SplitRow::Lines { .. }))
                .unwrap_or(0),
        }
    }

    pub fn layout(&self) -> DiffLayout {
        self.layout
    }

    /// The file shown.
    pub fn item(&self) -> Option<&DiffItem> {
        self.items.get(self.index)
    }

    #[cfg(test)]
    pub fn diff(&self) -> &FileDiff {
        &self.diff
    }

    /// The files that can be stepped through (tests).
    #[cfg(test)]
    pub fn items(&self) -> &[DiffItem] {
        &self.items
    }

    /// Rows in the current layout.
    pub fn row_count(&self) -> usize {
        match self.layout {
            DiffLayout::Unified => self.unified.len(),
            DiffLayout::Split => self.split.len(),
        }
    }

    pub fn set_layout(&mut self, layout: DiffLayout, cx: &mut Context<Self>) {
        self.layout = layout;
        self.reset_selection();
        cx.notify();
    }

    pub fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let count = self.items.len() as isize;
        let index = (self.index as isize + delta).rem_euclid(count) as usize;
        self.show(index);
        cx.notify();
    }

    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(revert) = self.item().and_then(|item| item.revert.clone()) {
            revert(window, cx);
        }
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .id("diff-files")
            .flex_shrink_0()
            .max_h(px(120.))
            .overflow_y_scroll()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(self.items.iter().enumerate().map(|(index, item)| {
                        let selected = index == self.index;
                        div()
                            .id(("diff-file", index))
                            .debug_selector(move || format!("diff-file-{index}"))
                            .w(px(220.))
                            .max_w_full()
                            .min_w_0()
                            .px_3()
                            .py_2()
                            .border_b_2()
                            .border_color(if selected {
                                colors.accent
                            } else {
                                colors.accent.opacity(0.)
                            })
                            .text_size(px(12.))
                            .text_color(if selected {
                                colors.text
                            } else {
                                colors.text_muted
                            })
                            .when(selected, |tab| tab.bg(colors.surface_hover))
                            .cursor_pointer()
                            .hover_style(|tab| tab.bg(colors.surface_hover).text_color(colors.text))
                            .tooltip(crate::ui::tooltip(item.display.clone()))
                            .child(div().truncate().child(item.display.clone()))
                            .when_some(item.status.label(), |tab, label| {
                                tab.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(colors.text_muted)
                                        .child(label),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.show(index);
                                cx.notify();
                            }))
                    })),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let item = self.item();
        let button = |id: &'static str, label: String, active: bool| {
            div()
                .id(id)
                .flex_shrink_0()
                .whitespace_nowrap()
                .px_2()
                .py_0p5()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(active, |this| {
                    this.bg(colors.surface_hover).text_color(colors.text)
                })
                .when(!active, |this| this.text_color(colors.text_muted))
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(label)
        };
        let stats = if self.diff.too_large {
            "too large to compare".to_owned()
        } else {
            format!("+{} −{}", self.diff.added, self.diff.removed)
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .child(
                button("diff-back", "← Back".into(), false)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DiffViewEvent::Close))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(item.map(|i| i.display.clone()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(item.map(|i| i.caption.clone()).unwrap_or_default()),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(stats),
            )
            .when_some(
                item.and_then(|item| item.status.label()),
                |header, label| {
                    header.child(
                        div()
                            .debug_selector(|| "diff-revert-status".into())
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(label),
                    )
                },
            )
            .when(self.items.len() > 1, |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .child(format!("{}/{}", self.index + 1, self.items.len())),
                )
                .child(
                    button("diff-prev", "‹".into(), false)
                        .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))),
                )
                .child(
                    button("diff-next", "›".into(), false)
                        .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .p_0p5()
                    .rounded(px(5.))
                    .bg(colors.background)
                    .child(
                        button(
                            "diff-unified",
                            "Unified".into(),
                            self.layout == DiffLayout::Unified,
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_layout(DiffLayout::Unified, cx)),
                        ),
                    )
                    .child(
                        button(
                            "diff-split",
                            "Split".into(),
                            self.layout == DiffLayout::Split,
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_layout(DiffLayout::Split, cx)),
                        ),
                    ),
            )
            .when(item.is_some_and(|i| i.revert.is_some()), |this| {
                this.child(
                    button("diff-revert", "Revert".into(), false)
                        .debug_selector(|| "diff-revert".into())
                        .text_color(colors.error)
                        .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx))),
                )
            })
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.selection.begin_frame();
        let colors = cx.theme().colors.clone();
        let diff_colors = DiffColors::of(cx);
        let body: AnyElement = if self.diff.too_large {
            div()
                .p_4()
                .text_color(colors.text_muted)
                .child("The file is too large to compare.")
                .into_any_element()
        } else if self.diff.is_empty() {
            div()
                .p_4()
                .text_color(colors.text_muted)
                .child("No differences.")
                .into_any_element()
        } else {
            let diff = self.diff.clone();
            let unified = self.unified.clone();
            let split = self.split.clone();
            let layout = self.layout;
            let count = self.row_count();
            let digit = digit_width(&diff_colors.mono, window);
            let gutters = Gutters::of(&diff, digit);
            let widths = SplitWidths::of(&split, gutters, digit);
            let selection = self.selection.clone();
            let path = self.items[self.index].path.to_string_lossy();
            let [old, new] = self.sources.each_ref().map(|source| {
                source
                    .as_ref()
                    .and_then(|source| source.highlight(&path, cx.theme().is_dark))
            });
            uniform_list("diff-rows", count, move |range, _, _| {
                range
                    .map(|ix| match layout {
                        DiffLayout::Unified => render_unified(
                            &unified[ix],
                            &diff_colors,
                            gutters,
                            Some((&selection, ix)),
                            [&old, &new],
                        ),
                        DiffLayout::Split => render_split_row(
                            &split[ix],
                            ix,
                            &diff,
                            &diff_colors,
                            widths,
                            &selection,
                            [&old, &new],
                        ),
                    })
                    .collect::<Vec<_>>()
            })
            .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(Some(self.widest_row()))
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        div()
            .id("diff-view")
            .key_context("DiffView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &CloseDiff, _, cx| cx.emit(DiffViewEvent::Close)))
            .on_action(cx.listener(
                |this, _: &gpui_elements::editable_text::actions::Copy, _, cx| {
                    if let Some(text) = this.selection.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                },
            ))
            .on_action(cx.listener(
                |this, _: &gpui_elements::editable_text::actions::SelectAll, window, _| {
                    this.selection.select_all();
                    window.refresh();
                },
            ))
            .on_mouse_move_all({
                let selection = self.selection.clone();
                move |event, phase, _, window, _| {
                    if phase == DispatchPhase::Bubble {
                        selection.extend_at(event.position, window);
                    }
                }
            })
            .on_mouse_up_all({
                let selection = self.selection.clone();
                move |event, phase, _, _, _| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        selection.end_drag();
                    }
                }
            })
            .on_action(cx.listener(|this, _: &NextDiffFile, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousDiffFile, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &ToggleDiffLayout, _, cx| {
                let next = match this.layout {
                    DiffLayout::Unified => DiffLayout::Split,
                    DiffLayout::Split => DiffLayout::Unified,
                };
                this.set_layout(next, cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .child(self.render_header(cx))
            .when(self.items.len() > 1, |view| {
                view.child(self.render_files(cx))
            })
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_ranges_follow_diff_lines_with_crlf_tabs_and_unicode() {
        let line = "\tlet café = \"你好\";";
        let text = Arc::<str>::from(format!("fn demo() {{\r\n{line}\r\n}}\r\n"));
        let source = DiffSource::new(&text).unwrap();
        let shown = expand_tabs(line);
        for dark in [false, true] {
            let highlights = source.highlight("src/demo.rs", dark).unwrap();
            let parts = highlights.line(2, line);
            assert!(
                parts
                    .iter()
                    .any(|(range, _)| &shown[range.clone()] == "let")
            );
            assert!(
                parts
                    .iter()
                    .any(|(range, _)| shown[range.clone()].contains("你好"))
            );
        }
    }

    #[test]
    fn unified_rows_have_headers() {
        let diff = FileDiff::compute(Some("a\nb\n"), Some("a\nc\n"), 3);
        let rows = unified_rows(&diff);
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0], UnifiedRow::Header(h) if h == "@@ -1,2 +1,2 @@"));
        assert!(matches!(&rows[2], UnifiedRow::Line(l) if l.kind == LineKind::Removed));
    }

    #[test]
    fn gutters_fit_the_numbers_and_skip_empty_columns() {
        let digit = px(8.);
        let created = FileDiff::compute(None, Some("a\nb\n"), 3);
        assert_eq!(
            Gutters::of(&created, digit),
            Gutters {
                old: None,
                new: Some(px(24.))
            }
        );
        let deleted = FileDiff::compute(Some("a\n"), None, 3);
        assert_eq!(Gutters::of(&deleted, digit).new, None);
        let long: String = (0..1200).map(|i| format!("{i}\n")).collect();
        let changed = FileDiff::compute(Some(&long), Some(&long.replace("1100\n", "x\n")), 3);
        let gutters = Gutters::of(&changed, digit);
        assert_eq!((gutters.old, gutters.new), (Some(px(48.)), Some(px(48.))));
        assert_eq!(display_columns("a中\tb"), 8);
    }
}
