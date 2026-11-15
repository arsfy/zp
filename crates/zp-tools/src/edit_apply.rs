//! Exact-text replacement for the `edit` tool (ported from pi's `edit-diff`).
//!
//! - Matching ignores line-ending style: the file and the edits are compared with LF line
//!   endings, and the result keeps the file's own line endings (and BOM).
//! - If an `old_text` is not found verbatim, a fuzzy match is tried that ignores trailing
//!   whitespace and treats typographic quotes, dashes and special spaces like their ASCII
//!   forms. Lines touched by a fuzzy edit are rewritten in normalized form; all other
//!   lines keep their bytes.
//! - Every edit must match exactly once, edits may not overlap, and the result must differ.

/// One replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditSpec {
    /// Text to find; must be unique in the file.
    pub old_text: String,
    /// Replacement.
    pub new_text: String,
}

/// A replacement of `len` bytes at `start` of some text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Replacement {
    start: usize,
    len: usize,
    new_text: String,
    edit: usize,
}

const BOM: &str = "\u{FEFF}";

/// Applies `edits` to `raw` file content. Errors are messages for the model.
pub fn apply_edits(raw: &str, edits: &[EditSpec], path: &str) -> Result<String, String> {
    let (bom, content) = match raw.strip_prefix(BOM) {
        Some(rest) => (BOM, rest),
        None => ("", raw),
    };
    let normalized = to_lf(content);
    let edits: Vec<EditSpec> = edits
        .iter()
        .map(|edit| EditSpec {
            old_text: to_lf(&edit.old_text),
            new_text: to_lf(&edit.new_text),
        })
        .collect();
    let total = edits.len();
    let name = |i: usize| {
        if total == 1 {
            "the text".to_owned()
        } else {
            format!("edits[{i}]")
        }
    };
    if total == 0 {
        return Err("edits must contain at least one replacement.".into());
    }
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(if total == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{i}].oldText must not be empty in {path}.")
            });
        }
    }

    let used_fuzzy = edits
        .iter()
        .any(|edit| !normalized.contains(&edit.old_text));
    let base = if used_fuzzy {
        fuzzy(&normalized)
    } else {
        normalized.clone()
    };
    let fuzzy_base = if used_fuzzy {
        base.clone()
    } else {
        fuzzy(&base)
    };

    let mut matched = Vec::with_capacity(total);
    for (i, edit) in edits.iter().enumerate() {
        let fuzzy_old = fuzzy(&edit.old_text);
        let (start, len) = match base.find(&edit.old_text) {
            Some(start) => (start, edit.old_text.len()),
            None => match base.find(&fuzzy_old) {
                Some(start) => (start, fuzzy_old.len()),
                None => {
                    return Err(if total == 1 {
                        format!(
                            "Could not find the exact text in {path}. The old text must match \
                             exactly including all whitespace and newlines."
                        )
                    } else {
                        format!(
                            "Could not find edits[{i}] in {path}. The oldText must match exactly \
                             including all whitespace and newlines."
                        )
                    });
                }
            },
        };
        let occurrences = fuzzy_base.matches(fuzzy_old.as_str()).count();
        if occurrences > 1 {
            return Err(format!(
                "Found {occurrences} occurrences of {} in {path}. {} must be unique. Please \
                 provide more context to make it unique.",
                name(i),
                if total == 1 {
                    "The text"
                } else {
                    "Each oldText"
                }
            ));
        }
        matched.push(Replacement {
            start,
            len,
            new_text: edit.new_text.clone(),
            edit: i,
        });
    }
    matched.sort_by_key(|r| r.start);
    for pair in matched.windows(2) {
        if pair[0].start + pair[0].len > pair[1].start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target \
                 disjoint regions.",
                pair[0].edit, pair[1].edit
            ));
        }
    }

    let replacements = if used_fuzzy {
        widen_to_lines(&normalized, &base, &matched)?
    } else {
        matched
    };
    let updated = apply(&normalized, &replacements);
    if updated == normalized {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This \
                 might indicate an issue with special characters or the text not existing as \
                 expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }
    let body = if content == normalized {
        updated
    } else {
        apply_to_original(content, &replacements)
    };
    Ok(format!("{bom}{body}"))
}

/// CRLF and lone CR become LF.
fn to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Normalization for fuzzy matching. Keeps the line count.
fn fuzzy(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            line.trim_end()
                .chars()
                .map(|c| match c {
                    '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                    '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                    '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                    '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => {
                        ' '
                    }
                    c => c,
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Applies sorted, disjoint replacements.
fn apply(text: &str, replacements: &[Replacement]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut position = 0;
    for r in replacements {
        out.push_str(&text[position..r.start]);
        out.push_str(&r.new_text);
        position = r.start + r.len;
    }
    out.push_str(&text[position..]);
    out
}

/// `(start, end)` byte spans of lines, each including its `\n`.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, _) in text.match_indices('\n') {
        spans.push((start, i + 1));
        start = i + 1;
    }
    if start < text.len() {
        spans.push((start, text.len()));
    }
    spans
}

/// Turns replacements found in the fuzzy `base` into whole-line replacements of
/// `original` (same line count), so untouched lines keep their exact text.
fn widen_to_lines(
    original: &str,
    base: &str,
    replacements: &[Replacement],
) -> Result<Vec<Replacement>, String> {
    let original_lines = line_spans(original);
    let base_lines = line_spans(base);
    if original_lines.len() != base_lines.len() {
        return Err("Cannot apply a fuzzy edit: line structure changed during matching.".into());
    }
    let line_of = |offset: usize| {
        base_lines
            .iter()
            .position(|&(s, e)| offset >= s && offset < e)
    };

    // Groups of replacements touching overlapping line ranges: (first, last+1, members).
    let mut groups: Vec<(usize, usize, Vec<&Replacement>)> = Vec::new();
    for r in replacements {
        let first = line_of(r.start).ok_or("Replacement range is outside the content.")?;
        let end = r.start + r.len;
        let last = (first..base_lines.len())
            .find(|&line| base_lines[line].1 >= end)
            .ok_or("Replacement range is outside the content.")?;
        match groups.last_mut() {
            Some(group) if first < group.1 => {
                group.1 = group.1.max(last + 1);
                group.2.push(r);
            }
            _ => groups.push((first, last + 1, vec![r])),
        }
    }
    Ok(groups
        .into_iter()
        .map(|(first, end, members)| {
            let (base_start, base_end) = (base_lines[first].0, base_lines[end - 1].1);
            let shifted: Vec<Replacement> = members
                .iter()
                .map(|r| Replacement {
                    start: r.start - base_start,
                    ..(*r).clone()
                })
                .collect();
            let original_start = original_lines[first].0;
            Replacement {
                start: original_start,
                len: original_lines[end - 1].1 - original_start,
                new_text: apply(&base[base_start..base_end], &shifted),
                edit: members[0].edit,
            }
        })
        .collect())
}

/// Applies replacements found in `to_lf(original)` to `original` itself, keeping its line
/// endings. New line breaks get the ending of the line where the replacement starts.
fn apply_to_original(original: &str, replacements: &[Replacement]) -> String {
    // Normalized index of the LF of each CRLF.
    let crlf_lf: Vec<usize> = original
        .match_indices("\r\n")
        .enumerate()
        .map(|(n, (i, _))| i - n)
        .collect();
    let to_original = |offset: usize| offset + crlf_lf.partition_point(|&lf| lf < offset);

    let mut out = String::with_capacity(original.len());
    let push = |part: &str, out: &mut String| {
        if part.is_empty() {
            return;
        }
        // A CR at a seam followed by an LF would merge into one CRLF; keep both breaks.
        if out.ends_with('\r') && part.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(part);
    };
    let mut position = 0;
    for r in replacements {
        let start = to_original(r.start);
        push(&original[position..start], &mut out);
        let ending = line_ending_at(original, start);
        if ending == "\n" {
            push(&r.new_text, &mut out);
        } else {
            push(&r.new_text.replace('\n', ending), &mut out);
        }
        position = to_original(r.start + r.len);
    }
    push(&original[position..], &mut out);
    out
}

/// The line ending used by the line containing `offset`.
fn line_ending_at(content: &str, offset: usize) -> &'static str {
    let lf = content[offset..]
        .find('\n')
        .map(|i| i + offset)
        .or_else(|| content[..offset].rfind('\n'));
    match lf {
        None if content.contains('\r') => "\r",
        None => "\n",
        Some(i) if i > 0 && content.as_bytes()[i - 1] == b'\r' => "\r\n",
        Some(_) => "\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> EditSpec {
        EditSpec {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn exact_single_and_multiple_edits() {
        let file = "fn a() {}\nfn b() {}\nfn c() {}\n";
        assert_eq!(
            apply_edits(file, &[edit("fn b() {}", "fn b() { 1 }")], "f.rs").unwrap(),
            "fn a() {}\nfn b() { 1 }\nfn c() {}\n"
        );
        assert_eq!(
            apply_edits(file, &[edit("fn c", "fn z"), edit("fn a", "fn y")], "f.rs").unwrap(),
            "fn y() {}\nfn b() {}\nfn z() {}\n"
        );
    }

    #[test]
    fn errors() {
        let file = "x = 1\nx = 1\ny = 2\n";
        assert!(
            apply_edits(file, &[edit("x = 1", "x = 3")], "f")
                .unwrap_err()
                .starts_with("Found 2 occurrences of the text in f.")
        );
        assert!(
            apply_edits(file, &[edit("nope", "")], "f")
                .unwrap_err()
                .starts_with("Could not find the exact text in f.")
        );
        assert!(
            apply_edits(file, &[edit("y", "y"), edit("q", "")], "f")
                .unwrap_err()
                .starts_with("Could not find edits[1] in f.")
        );
        assert!(
            apply_edits(file, &[edit("", "a")], "f")
                .unwrap_err()
                .starts_with("oldText must not be empty")
        );
        assert!(
            apply_edits(file, &[edit("y = 2", "y = 2")], "f")
                .unwrap_err()
                .starts_with("No changes made to f.")
        );
        assert!(
            apply_edits(file, &[edit("1\ny", "1\nz"), edit("y = 2", "w")], "f")
                .unwrap_err()
                .contains("overlap")
        );
        assert!(apply_edits(file, &[], "f").is_err());
    }

    #[test]
    fn keeps_crlf_and_bom() {
        let file = "\u{FEFF}one\r\ntwo\r\nthree\r\n";
        assert_eq!(
            apply_edits(file, &[edit("two\nthree", "2\n3\n3.5")], "f").unwrap(),
            "\u{FEFF}one\r\n2\r\n3\r\n3.5\r\n"
        );
        // Mixed endings: untouched lines keep theirs.
        let mixed = "a\r\nb\nc\r\n";
        assert_eq!(
            apply_edits(mixed, &[edit("c", "C")], "f").unwrap(),
            "a\r\nb\nC\r\n"
        );
    }

    #[test]
    fn fuzzy_matching_rewrites_only_touched_lines() {
        let file = "keep   \nlet s = \u{201C}hi\u{201D};   \nnext\u{00A0}\n";
        let out = apply_edits(file, &[edit("let s = \"hi\";", "let s = \"bye\";")], "f").unwrap();
        // The edited line is normalized; untouched lines keep their trailing whitespace.
        assert_eq!(out, "keep   \nlet s = \"bye\";\nnext\u{00A0}\n");

        let dashes = "a \u{2014} b\n";
        assert_eq!(
            apply_edits(dashes, &[edit("a - b", "a + b")], "f").unwrap(),
            "a + b\n"
        );
    }

    #[test]
    fn seam_between_cr_and_lf_keeps_both_breaks() {
        // Lone CR in a file that otherwise uses LF stays a separate break.
        let file = "a\rb\nc\n";
        let out = apply_edits(file, &[edit("b\n", "")], "f").unwrap();
        assert_eq!(to_lf(&out), "a\nc\n");
    }
}
