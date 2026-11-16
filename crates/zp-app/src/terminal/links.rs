//! Link detection uses the emulator's search so wide cells and soft wraps stay aligned.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Boundary, Direction, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Link {
    pub range: Match,
    pub url: String,
}

pub(super) struct Detector(RegexSearch);

impl Default for Detector {
    fn default() -> Self {
        Self(
            RegexSearch::new(r#"(?i:(https?|ftp|file)://|mailto:|www\.)[^\s<>"'\x00-\x1f\x7f]+"#)
                .unwrap(),
        )
    }
}

fn openable(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, rest)| {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ftp" | "file" | "mailto"
        ) && !rest.is_empty()
            && !url.chars().any(char::is_control)
    })
}

impl Detector {
    pub fn at<T: EventListener>(&mut self, term: &Term<T>, point: Point) -> Option<Link> {
        if point.line < term.topmost_line() || point.line > term.bottommost_line() {
            return None;
        }
        if let Some(hyperlink) = term.grid()[point]
            .hyperlink()
            .filter(|link| openable(link.uri()))
        {
            let (mut start, mut end) = (point, point);
            let first = Point::new(term.topmost_line(), Default::default());
            let last = Point::new(term.bottommost_line(), term.last_column());
            while start > first {
                let previous = start.sub(term, Boundary::Grid, 1);
                if term.grid()[previous].hyperlink().as_ref() != Some(&hyperlink) {
                    break;
                }
                start = previous;
            }
            while end < last {
                let next = end.add(term, Boundary::Grid, 1);
                if term.grid()[next].hyperlink().as_ref() != Some(&hyperlink) {
                    break;
                }
                end = next;
            }
            return Some(Link {
                range: start..=end,
                url: hyperlink.uri().to_owned(),
            });
        }

        let start = term.line_search_left(point);
        let end = term.line_search_right(point);
        let ranges: Vec<_> =
            RegexIter::new(start, end, Direction::Right, term, &mut self.0).collect();
        for range in ranges {
            if !range.contains(&point) {
                continue;
            }
            let start = *range.start();
            let mut end = *range.end();
            let mut url = term.bounds_to_string(start, end);
            // Keep balanced URL parentheses, while dropping prose punctuation around links.
            while let Some(last) = url.chars().last() {
                let trim = matches!(
                    last,
                    '.' | ',' | ';' | ':' | '!' | '?' | '\u{3002}' | '\u{ff0c}'
                ) || [(')', '('), (']', '['), ('}', '{')].iter().any(
                    |&(close, open)| {
                        last == close && url.matches(close).count() > url.matches(open).count()
                    },
                );
                if !trim {
                    break;
                }
                url.truncate(url.len() - last.len_utf8());
                end = term
                    .expand_wide(end, Direction::Left)
                    .sub(term, Boundary::Grid, 1);
            }
            if url.to_ascii_lowercase().starts_with("www.") {
                url.insert_str(0, "https://");
            }
            return (end >= point && openable(&url)).then_some(Link {
                range: start..=end,
                url,
            });
        }
        None
    }
}
