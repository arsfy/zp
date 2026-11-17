//! Diffs of files: the full-size diff view (unified or side by side) and the line
//! renderers shared with the inline diffs of tool cards.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ColorExt as _, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, Render, Rgba, SharedString, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};
use zp_core::changes::RevertStatus;
use zp_core::diff::{DEFAULT_CONTEXT, DiffLine, FileDiff, LineKind, SideLine, SplitRow};

use crate::actions::{CloseDiff, NextDiffFile, PreviousDiffFile, ToggleDiffLayout};
use crate::theme::ActiveTheme;

/// Height of one diff row.
const ROW_HEIGHT: f32 = 20.;

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
    pub status: RevertStatus,
    /// Offered as "Revert" when set.
    pub revert: Option<RevertFn>,
}

impl DiffItem {
    pub fn diff(&self) -> FileDiff {
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

fn gutter(number: Option<usize>, colors: &DiffColors) -> impl IntoElement {
    div()
        .w(px(44.))
        .flex_shrink_0()
        .pr_2()
        .flex()
        .justify_end()
        .text_color(colors.muted)
        .child(number.map(|n| n.to_string()).unwrap_or_default())
}

fn code(text: &str) -> impl IntoElement {
    // Tabs would collapse in a single-line text run.
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .child(text.replace('\t', "    "))
}

/// A unified diff row.
pub fn render_unified_row(row: &UnifiedRow, colors: &DiffColors) -> AnyElement {
    let base = div()
        .h(px(ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .font_family(colors.mono.clone())
        .text_size(px(12.));
    match row {
        UnifiedRow::Header(header) => base
            .bg(colors.header_bg)
            .text_color(colors.muted)
            .pl(px(96.))
            .child(header.clone())
            .into_any_element(),
        UnifiedRow::Line(line) => {
            let (sign, color) = match line.kind {
                LineKind::Added => ("+", colors.added),
                LineKind::Removed => ("-", colors.removed),
                LineKind::Context => (" ", colors.muted),
            };
            base.when_some(colors.line_bg(line.kind), |this, bg| this.bg(bg))
                .text_color(colors.text)
                .child(gutter(line.old, colors))
                .child(gutter(line.new, colors))
                .child(
                    div()
                        .w(px(14.))
                        .flex_shrink_0()
                        .text_color(color)
                        .child(sign),
                )
                .child(code(&line.text))
                .into_any_element()
        }
    }
}

fn render_side(side: Option<&SideLine>, colors: &DiffColors) -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .when_some(side.and_then(|s| colors.line_bg(s.kind)), |this, bg| {
            this.bg(bg)
        })
        .when(side.is_none(), |this| this.bg(colors.muted.opacity(0.05)))
        .when_some(side, |this, side| {
            this.child(gutter(Some(side.number), colors))
                .child(code(&side.text))
        })
}

/// A side-by-side diff row.
pub fn render_split_row(row: &SplitRow, diff: &FileDiff, colors: &DiffColors) -> AnyElement {
    let base = div()
        .h(px(ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .font_family(colors.mono.clone())
        .text_size(px(12.))
        .text_color(colors.text);
    match row {
        SplitRow::Header(index) => base
            .bg(colors.header_bg)
            .text_color(colors.muted)
            .pl(px(52.))
            .child(diff.hunks[*index].header())
            .into_any_element(),
        SplitRow::Lines { left, right } => base
            .child(render_side(left.as_ref(), colors))
            .child(div().w(px(1.)).h_full().bg(colors.muted.opacity(0.3)))
            .child(render_side(right.as_ref(), colors))
            .into_any_element(),
    }
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
    scroll: UniformListScrollHandle,
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
        let mut this = Self {
            items,
            index: 0,
            layout,
            diff: Rc::default(),
            unified: Rc::default(),
            split: Rc::default(),
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
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
        self.diff = Rc::new(diff);
        self.scroll = UniformListScrollHandle::new();
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            uniform_list("diff-rows", count, move |range, _, _| {
                range
                    .map(|ix| match layout {
                        DiffLayout::Unified => render_unified_row(&unified[ix], &diff_colors),
                        DiffLayout::Split => render_split_row(&split[ix], &diff, &diff_colors),
                    })
                    .collect::<Vec<_>>()
            })
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        div()
            .id("diff-view")
            .key_context("DiffView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &CloseDiff, _, cx| cx.emit(DiffViewEvent::Close)))
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
    fn unified_rows_have_headers() {
        let diff = FileDiff::compute(Some("a\nb\n"), Some("a\nc\n"), 3);
        let rows = unified_rows(&diff);
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0], UnifiedRow::Header(h) if h == "@@ -1,2 +1,2 @@"));
        assert!(matches!(&rows[2], UnifiedRow::Line(l) if l.kind == LineKind::Removed));
    }
}
