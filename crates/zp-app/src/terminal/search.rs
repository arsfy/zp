//! Literal, case-insensitive search over the terminal's screen and scrollback.

use std::rc::Rc;
use std::time::Duration;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use gpui::{
    AnyElement, Context, Focusable as _, IntoElement, Window, div, prelude::*, px, rgb_to_hsla, svg,
};
use gpui_elements::editable_text::text_input;

use super::TerminalView;
use crate::theme::ActiveTheme;

/// How long new output waits before it is searched: output arrives in many small batches,
/// and each search scans the whole scrollback.
pub(super) const OUTPUT_SEARCH_DELAY: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(super) struct Search {
    /// Shared with the frame being painted, which would otherwise copy it every frame.
    pub matches: Rc<Vec<Match>>,
    pub current: usize,
    query: String,
    regex: Option<RegexSearch>,
}

impl TerminalView {
    pub(super) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() {
            self.search = Some(Search::default());
            self.refresh_search(true, cx);
        }
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    pub(super) fn refresh_search(&mut self, reset: bool, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            let mut term = self.term.lock();
            search.refresh(&term, self.search_input.read(cx).as_str(), reset);
            if reset && let Some(range) = search.active() {
                term.scroll_to_point(*range.start());
            }
            drop(term);
            cx.notify();
        }
    }

    /// Searches again after new output, at most once per [`OUTPUT_SEARCH_DELAY`].
    /// Typing in the search bar still searches right away.
    pub(super) fn schedule_output_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() || self.pending_output_search.is_some() {
            return;
        }
        self.pending_output_search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(OUTPUT_SEARCH_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.pending_output_search = None;
                this.refresh_search(false, cx);
            });
        }));
    }

    pub(super) fn next_match(&mut self, backwards: bool, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.advance(backwards);
            if let Some(range) = search.active() {
                self.term.lock().scroll_to_point(*range.start());
            }
            cx.notify();
        }
    }

    pub(super) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self.pending_output_search = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    pub(super) fn render_search(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let search = self.search.as_ref()?;
        let colors = cx.theme().colors.clone();
        let icon = |id, data: &'static [u8], label| {
            div()
                .id(id)
                .size(px(24.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .hover_style(|style| style.bg(colors.surface_hover))
                .tooltip(crate::ui::tooltip(label))
                .child(svg().data(data).size(px(14.)).text_color(colors.text_muted))
        };
        let count = if search.matches.is_empty() {
            "0 / 0".to_owned()
        } else {
            format!("{} / {}", search.current + 1, search.matches.len())
        };
        Some(
            div()
                .key_context("TerminalSearch")
                .flex()
                .items_center()
                .flex_shrink_0()
                .gap_1()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .child(
                    text_input("terminal-search-input")
                        .state(self.search_input.downgrade())
                        .placeholder("Find in terminal")
                        .placeholder_color(rgb_to_hsla(colors.text_muted))
                        .caret_color(rgb_to_hsla(colors.text))
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .whitespace_nowrap(),
                )
                .child(
                    div()
                        .w(px(80.))
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child(count),
                )
                .child(
                    icon(
                        "terminal-search-previous",
                        include_bytes!("../../../../assets/icons/chevron-up.svg"),
                        "Previous match",
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.next_match(true, cx))),
                )
                .child(
                    icon(
                        "terminal-search-next",
                        include_bytes!("../../../../assets/icons/chevron-down.svg"),
                        "Next match",
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.next_match(false, cx))),
                )
                .child(
                    icon(
                        "terminal-search-close",
                        include_bytes!("../../../../assets/icons/x.svg"),
                        "Close search",
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
                )
                .into_any_element(),
        )
    }
}

impl Search {
    pub fn refresh<T: EventListener>(&mut self, term: &Term<T>, query: &str, reset: bool) {
        let anchor = if reset {
            Point::new(Line(-(term.grid().display_offset() as i32)), Column(0))
        } else {
            self.active()
                .map(|range| *range.start())
                .unwrap_or_default()
        };
        let mut matches = Vec::new();
        if self.query != query {
            self.query = query.to_owned();
            self.regex = (!query.is_empty())
                .then(|| RegexSearch::new(&format!("(?i:{})", regex::escape(query))).ok())
                .flatten();
        }
        if let Some(regex) = self.regex.as_mut() {
            matches = RegexIter::new(
                Point::new(term.topmost_line(), Column(0)),
                Point::new(term.bottommost_line(), term.last_column()),
                Direction::Right,
                term,
                regex,
            )
            .take(10_000)
            .collect();
        }
        self.matches = Rc::new(matches);
        self.current = self
            .matches
            .iter()
            .position(|range| *range.start() >= anchor)
            .unwrap_or(0);
    }

    pub fn active(&self) -> Option<&Match> {
        self.matches.get(self.current)
    }

    pub fn advance(&mut self, backwards: bool) {
        let count = self.matches.len();
        if count > 0 {
            self.current = if backwards {
                (self.current + count - 1) % count
            } else {
                (self.current + 1) % count
            };
        }
    }
}
