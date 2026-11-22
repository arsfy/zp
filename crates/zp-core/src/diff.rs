//! Line diffs of text files, as unified hunks or side-by-side rows.

use std::time::Duration;

use similar::{ChangeTag, TextDiff};

/// Lines of unchanged context around each change in a hunk.
pub const DEFAULT_CONTEXT: usize = 3;

/// Inputs larger than this are not diffed line by line (summarized instead).
pub const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;

/// Gives up on a minimal diff after this long (the result is still correct, just
/// coarser).
const DIFF_DEADLINE: Duration = Duration::from_millis(500);

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One line of a hunk. Line numbers are 1-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the old version (context and removed lines).
    pub old: Option<usize>,
    /// Line number in the new version (context and added lines).
    pub new: Option<usize>,
    /// The text, without the line ending.
    pub text: String,
}

/// A run of changes with surrounding context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// First old line (1-based; 0 for an empty old side).
    pub old_start: usize,
    pub old_len: usize,
    /// First new line (1-based; 0 for an empty new side).
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// `@@ -a,b +c,d @@`.
    pub fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

/// One side of a side-by-side row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideLine {
    pub number: usize,
    pub kind: LineKind,
    pub text: String,
}

/// A row of a side-by-side diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SplitRow {
    /// Start of a hunk (index into [`FileDiff::hunks`]).
    Header(usize),
    /// Old and new lines side by side; a side is empty where lines were only added or
    /// only removed.
    Lines {
        left: Option<SideLine>,
        right: Option<SideLine>,
    },
}

/// The difference between two versions of a text file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
    /// The inputs were too large to diff by line.
    pub too_large: bool,
}

impl FileDiff {
    /// Diffs `before` (`None`: the file did not exist) against `after` (`None`: deleted).
    pub fn compute(before: Option<&str>, after: Option<&str>, context: usize) -> Self {
        let old = before.unwrap_or_default();
        let new = after.unwrap_or_default();
        if old.len() + new.len() > MAX_DIFF_BYTES {
            return Self {
                too_large: true,
                ..Self::default()
            };
        }
        let diff = TextDiff::configure()
            .timeout(DIFF_DEADLINE)
            .diff_lines(old, new);
        let mut result = Self::default();
        for group in diff.grouped_ops(context) {
            let (Some(first), Some(last)) = (group.first(), group.last()) else {
                continue;
            };
            let old_range = first.old_range().start..last.old_range().end;
            let new_range = first.new_range().start..last.new_range().end;
            let mut hunk = Hunk {
                old_start: if old_range.is_empty() {
                    old_range.start
                } else {
                    old_range.start + 1
                },
                old_len: old_range.len(),
                new_start: if new_range.is_empty() {
                    new_range.start
                } else {
                    new_range.start + 1
                },
                new_len: new_range.len(),
                lines: Vec::new(),
            };
            for op in &group {
                for change in diff.iter_changes(op) {
                    let kind = match change.tag() {
                        ChangeTag::Equal => LineKind::Context,
                        ChangeTag::Insert => {
                            result.added += 1;
                            LineKind::Added
                        }
                        ChangeTag::Delete => {
                            result.removed += 1;
                            LineKind::Removed
                        }
                    };
                    hunk.lines.push(DiffLine {
                        kind,
                        old: change.old_index().map(|i| i + 1),
                        new: change.new_index().map(|i| i + 1),
                        text: strip_newline(change.value()).to_owned(),
                    });
                }
            }
            result.hunks.push(hunk);
        }
        result
    }

    /// Whether the versions are identical.
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty() && !self.too_large
    }

    /// Number of lines over all hunks.
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|hunk| hunk.lines.len()).sum()
    }

    /// Side-by-side rows: context on both sides, removed lines paired with the added
    /// lines that replace them.
    pub fn split_rows(&self) -> Vec<SplitRow> {
        let mut rows = Vec::new();
        for (index, hunk) in self.hunks.iter().enumerate() {
            rows.push(SplitRow::Header(index));
            let mut removed: Vec<SideLine> = Vec::new();
            let mut added: Vec<SideLine> = Vec::new();
            let flush = |rows: &mut Vec<SplitRow>,
                         removed: &mut Vec<SideLine>,
                         added: &mut Vec<SideLine>| {
                let count = removed.len().max(added.len());
                let mut left = removed.drain(..);
                let mut right = added.drain(..);
                for _ in 0..count {
                    rows.push(SplitRow::Lines {
                        left: left.next(),
                        right: right.next(),
                    });
                }
            };
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Removed => removed.push(SideLine {
                        number: line.old.unwrap_or_default(),
                        kind: LineKind::Removed,
                        text: line.text.clone(),
                    }),
                    LineKind::Added => added.push(SideLine {
                        number: line.new.unwrap_or_default(),
                        kind: LineKind::Added,
                        text: line.text.clone(),
                    }),
                    LineKind::Context => {
                        flush(&mut rows, &mut removed, &mut added);
                        rows.push(SplitRow::Lines {
                            left: Some(SideLine {
                                number: line.old.unwrap_or_default(),
                                kind: LineKind::Context,
                                text: line.text.clone(),
                            }),
                            right: Some(SideLine {
                                number: line.new.unwrap_or_default(),
                                kind: LineKind::Context,
                                text: line.text.clone(),
                            }),
                        });
                    }
                }
            }
            flush(&mut rows, &mut removed, &mut added);
        }
        rows
    }

    /// The diff as unified-diff text (without file headers).
    pub fn unified_text(&self) -> String {
        let mut out = String::new();
        for hunk in &self.hunks {
            out.push_str(&hunk.header());
            out.push('\n');
            for line in &hunk.lines {
                out.push(match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                });
                out.push_str(&line.text);
                out.push('\n');
            }
        }
        out
    }
}

/// `(added, removed)` line counts between two versions.
pub fn line_stats(before: Option<&str>, after: Option<&str>) -> (usize, usize) {
    let diff = FileDiff::compute(before, after, 0);
    (diff.added, diff.removed)
}

fn strip_newline(text: &str) -> &str {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.strip_suffix('\r').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_and_counts() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let after = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let diff = FileDiff::compute(Some(before), Some(after), 1);
        assert_eq!((diff.added, diff.removed), (2, 1));
        assert_eq!(diff.hunks.len(), 2);
        assert_eq!(diff.hunks[0].header(), "@@ -1,3 +1,3 @@");
        assert_eq!(
            diff.unified_text(),
            "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -10,1 +10,2 @@\n j\n+k\n"
        );
        let kinds: Vec<LineKind> = diff.hunks[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context
            ]
        );
        assert_eq!(diff.hunks[0].lines[2].new, Some(2));
        assert_eq!(diff.hunks[0].lines[1].old, Some(2));
    }

    #[test]
    fn created_deleted_and_identical() {
        let created = FileDiff::compute(None, Some("x\ny"), 3);
        assert_eq!((created.added, created.removed), (2, 0));
        assert_eq!(created.hunks[0].header(), "@@ -0,0 +1,2 @@");
        let deleted = FileDiff::compute(Some("x\n"), None, 3);
        assert_eq!((deleted.added, deleted.removed), (0, 1));
        assert!(FileDiff::compute(Some("same\n"), Some("same\n"), 3).is_empty());
        assert_eq!(line_stats(Some("a\r\nb\r\n"), Some("a\r\nc\r\n")), (1, 1));
    }

    #[test]
    fn split_rows_pair_replacements() {
        let diff = FileDiff::compute(Some("a\nb\nc\nd\n"), Some("a\nB1\nB2\nd\n"), 3);
        let rows = diff.split_rows();
        let side = |s: &Option<SideLine>| s.as_ref().map(|l| (l.number, l.text.clone()));
        let shape: Vec<_> = rows
            .iter()
            .map(|row| match row {
                SplitRow::Header(i) => (Some((*i, "@@".to_owned())), None),
                SplitRow::Lines { left, right } => (side(left), side(right)),
            })
            .collect();
        assert_eq!(
            shape,
            [
                (Some((0, "@@".into())), None),
                (Some((1, "a".into())), Some((1, "a".into()))),
                (Some((2, "b".into())), Some((2, "B1".into()))),
                (Some((3, "c".into())), Some((3, "B2".into()))),
                (Some((4, "d".into())), Some((4, "d".into()))),
            ]
        );
        // More additions than removals leave the left side empty.
        let diff = FileDiff::compute(Some("a\n"), Some("a\nb\nc\n"), 0);
        let rows = diff.split_rows();
        assert!(
            matches!(&rows[1], SplitRow::Lines { left: None, right: Some(r) } if r.text == "b")
        );
    }
}
