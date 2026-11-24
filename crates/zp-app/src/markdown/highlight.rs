//! Syntax highlighting of code blocks: Sublime grammars through syntect (bat's set, via
//! two-face), colored by a light or dark editor theme.
//!
//! Results are cached by content. A block that is still being streamed only grows, so it
//! resumes from the parser state at its last complete line, which is cheap enough to do
//! while drawing. Anything else (a session opened with long code blocks, the first block
//! of a language) is highlighted on a background thread: the block shows plain until the
//! result is in, then the windows are redrawn.

use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use futures::StreamExt as _;
use gpui::{App, FontStyle, FontWeight, HighlightStyle, Hsla, UnderlineStyle, px, rgb_to_hsla};
use syntect::highlighting::{
    FontStyle as ThemeFontStyle, HighlightIterator, HighlightState, Highlighter, Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use two_face::theme::EmbeddedThemeName;

/// Highlighted ranges of a block's text.
pub type Highlights = Arc<Vec<(Range<usize>, HighlightStyle)>>;

/// Blocks longer than this are shown plain (highlighting cost grows with the text).
pub(crate) const MAX_HIGHLIGHTED_BYTES: usize = 256 * 1024;
/// Cached results and resumable states kept at most.
const MAX_CACHED: usize = 256;
const MAX_CHECKPOINTS: usize = 16;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
static THEMES: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);
/// Asks the app to redraw once background results are in. Unset (tests), everything is
/// highlighted right away.
static REDRAW: OnceLock<futures::channel::mpsc::UnboundedSender<()>> = OnceLock::new();

#[derive(Default)]
struct Cache {
    done: HashMap<u64, Highlights>,
    checkpoints: Vec<Checkpoint>,
    /// Texts being highlighted in the background, by grammar and background.
    in_flight: Vec<(String, bool, String)>,
}

/// Where highlighting of a growing text can resume: everything up to the end of its last
/// complete line is done.
struct Checkpoint {
    syntax: String,
    dark: bool,
    text: String,
    parse: ParseState,
    highlight: HighlightState,
    spans: Vec<(Range<usize>, HighlightStyle)>,
}

fn lock() -> std::sync::MutexGuard<'static, Cache> {
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Redraws the windows whenever background highlighting finishes.
///
/// Not in tests: the sender is process-wide, and a background thread waking a task of one
/// test's scheduler aborts the test run. Tests highlight right away instead.
pub fn init(cx: &mut App) {
    if cfg!(test) {
        return;
    }
    let (sender, mut receiver) = futures::channel::mpsc::unbounded();
    if REDRAW.set(sender).is_err() {
        return;
    }
    cx.spawn(async move |cx| {
        while receiver.next().await.is_some() {
            // One redraw for everything that finished meanwhile.
            while receiver.try_recv().is_ok() {}
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();
}

/// Names people put after the fence that the grammars don't know by themselves.
fn alias(language: &str) -> &str {
    match language {
        "shell" | "sh" | "zsh" | "console" | "shellsession" => "bash",
        "ps" | "ps1" | "pwsh" => "powershell",
        "jsonc" | "json5" => "json",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "py" | "python3" => "python",
        "rs" => "rust",
        "golang" => "go",
        "c++" | "cxx" | "hpp" => "cpp",
        "objc" | "objective-c" => "objective-c",
        "docker" => "dockerfile",
        "yml" => "yaml",
        "md" => "markdown",
        "kt" | "kts" => "kotlin",
        "tf" | "hcl" => "terraform",
        other => other,
    }
}

/// The grammar for a fence's language (`rust`, `rs`, `TypeScript`, `Cargo.toml`…).
fn syntax_for(language: &str) -> Option<&'static SyntaxReference> {
    let language = language.trim().to_ascii_lowercase();
    let language = alias(&language);
    if matches!(language, "" | "text" | "plain" | "plaintext" | "txt") {
        return None;
    }
    let syntaxes = &*SYNTAXES;
    syntaxes
        .find_syntax_by_token(language)
        .or_else(|| {
            // A file name or path: by extension, or by the whole name (`Makefile`).
            let name = language.rsplit(['/', '\\']).next().unwrap_or(language);
            name.rsplit_once('.')
                .and_then(|(_, extension)| syntaxes.find_syntax_by_extension(extension))
                .or_else(|| syntaxes.find_syntax_by_extension(name))
        })
        .filter(|syntax| syntax.name != "Plain Text")
}

fn theme(dark: bool) -> &'static Theme {
    THEMES.get(if dark {
        EmbeddedThemeName::OneHalfDark
    } else {
        EmbeddedThemeName::OneHalfLight
    })
}

/// Highlights `text` written in `language`, for a dark or light background. `None` when
/// the language is unknown, the block too large, or the result is still being computed
/// (the windows are redrawn when it is ready); the text is then shown plain.
pub fn highlight(language: Option<&str>, text: &str, dark: bool) -> Option<Highlights> {
    highlight_with(language, text, dark, REDRAW.get())
}

/// [`highlight`], in the background (then redrawing through `redraw`) or right away.
fn highlight_with(
    language: Option<&str>,
    text: &str,
    dark: bool,
    redraw: Option<&futures::channel::mpsc::UnboundedSender<()>>,
) -> Option<Highlights> {
    let syntax = syntax_for(language?)?;
    if text.len() > MAX_HIGHLIGHTED_BYTES {
        return None;
    }
    let key = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (&syntax.name, dark, text).hash(&mut hasher);
        hasher.finish()
    };
    let mut cache = lock();
    if let Some(done) = cache.done.get(&key) {
        return Some(done.clone());
    }
    // Resume from the longest checkpoint this text extends (a streaming block).
    let resume = cache
        .checkpoints
        .iter()
        .enumerate()
        .filter(|(_, c)| c.syntax == syntax.name && c.dark == dark && text.starts_with(&c.text))
        .max_by_key(|(_, c)| c.text.len())
        .map(|(index, _)| index)
        .map(|index| cache.checkpoints.swap_remove(index));

    if resume.is_none()
        && let Some(redraw) = redraw
    {
        let waiting = cache.in_flight.iter().any(|(name, in_dark, started)| {
            *name == syntax.name && *in_dark == dark && text.starts_with(started.as_str())
        });
        if !waiting {
            let job = (syntax.name.clone(), dark, text.to_owned());
            cache.in_flight.push(job.clone());
            let redraw = redraw.clone();
            std::thread::spawn(move || {
                let (name, dark, text) = job;
                if let Some(syntax) = SYNTAXES.find_syntax_by_name(&name) {
                    let (highlights, checkpoint) = compute(syntax, dark, &text, None);
                    store(key, highlights, checkpoint);
                }
                lock()
                    .in_flight
                    .retain(|(n, d, t)| !(*n == name && *d == dark && *t == text));
                let _ = redraw.unbounded_send(());
            });
        }
        return None;
    }
    drop(cache);
    let (highlights, checkpoint) = compute(syntax, dark, text, resume);
    store(key, highlights.clone(), checkpoint);
    Some(highlights)
}

fn store(key: u64, highlights: Highlights, checkpoint: Option<Checkpoint>) {
    let mut cache = lock();
    if let Some(checkpoint) = checkpoint {
        cache.checkpoints.push(checkpoint);
        if cache.checkpoints.len() > MAX_CHECKPOINTS {
            cache.checkpoints.remove(0);
        }
    }
    if cache.done.len() >= MAX_CACHED {
        cache.done.clear();
    }
    cache.done.insert(key, highlights);
}

/// Highlights `text`, from `resume` if given; also returns the state at its last complete
/// line. A grammar error leaves the rest plain.
fn compute(
    syntax: &SyntaxReference,
    dark: bool,
    text: &str,
    resume: Option<Checkpoint>,
) -> (Highlights, Option<Checkpoint>) {
    let theme = theme(dark);
    let highlighter = Highlighter::new(theme);
    let default_color = theme.settings.foreground;
    let (mut parse, mut state, mut spans, start) = match resume {
        Some(checkpoint) => {
            let start = checkpoint.text.len();
            (
                checkpoint.parse,
                checkpoint.highlight,
                checkpoint.spans,
                start,
            )
        }
        None => (
            ParseState::new(syntax),
            HighlightState::new(&highlighter, ScopeStack::new()),
            Vec::new(),
            0,
        ),
    };

    let mut offset = start;
    let mut checkpoint = None;
    for line in syntect::util::LinesWithEndings::from(&text[start..]) {
        let Ok(ops) = parse.parse_line(line, &SYNTAXES) else {
            break;
        };
        for (style, piece) in HighlightIterator::new(&mut state, &ops, line, &highlighter) {
            let range = offset..offset + piece.len();
            offset += piece.len();
            if let Some(highlight) = to_highlight(style, default_color)
                && !piece.trim().is_empty()
            {
                spans.push((range, highlight));
            }
        }
        if line.ends_with('\n') {
            checkpoint = Some((offset, parse.clone(), state.clone(), spans.len()));
        }
    }
    let checkpoint = checkpoint.map(|(end, parse, highlight, count)| Checkpoint {
        syntax: syntax.name.clone(),
        dark,
        text: text[..end].to_owned(),
        parse,
        highlight,
        spans: spans[..count].to_vec(),
    });
    (Arc::new(spans), checkpoint)
}

/// The theme style as a text highlight; `None` when it is plain text.
fn to_highlight(
    style: syntect::highlighting::Style,
    default_color: Option<syntect::highlighting::Color>,
) -> Option<HighlightStyle> {
    let mut highlight = HighlightStyle::default();
    let color = style.foreground;
    if Some(color) != default_color {
        highlight.color = Some(to_hsla(color));
    }
    if style.font_style.contains(ThemeFontStyle::BOLD) {
        highlight.font_weight = Some(FontWeight::BOLD);
    }
    if style.font_style.contains(ThemeFontStyle::ITALIC) {
        highlight.font_style = Some(FontStyle::Italic);
    }
    if style.font_style.contains(ThemeFontStyle::UNDERLINE) {
        highlight.underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        });
    }
    (highlight != HighlightStyle::default()).then_some(highlight)
}

fn to_hsla(color: syntect::highlighting::Color) -> Hsla {
    rgb_to_hsla(gpui::rgba(u32::from_be_bytes([
        color.r, color.g, color.b, color.a,
    ])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colored(highlights: &Highlights, text: &str, token: &str) -> Option<Hsla> {
        let start = text.find(token).unwrap();
        highlights
            .iter()
            .find(|(range, _)| range.start <= start && start < range.end)
            .and_then(|(_, style)| style.color)
    }

    #[test]
    fn languages_by_name_alias_and_file_name() {
        for language in [
            "rust",
            "rs",
            "Rust",
            "python",
            "py",
            "ts",
            "typescript",
            "tsx",
            "go",
            "toml",
            "yaml",
            "sh",
            "shell",
            "json",
            "c",
            "cpp",
            "java",
            "kotlin",
            "swift",
            "ruby",
            "sql",
            "html",
            "css",
            "diff",
            "Dockerfile",
            "Makefile",
            "src/main.rs",
            "Cargo.toml",
        ] {
            assert!(syntax_for(language).is_some(), "{language}");
        }
        for plain in ["", "text", "plaintext", "no-such-language"] {
            assert!(syntax_for(plain).is_none(), "{plain}");
        }
    }

    #[test]
    fn rust_keywords_strings_and_comments_get_colors() {
        let text = "// add two numbers\nfn add(a: i32) -> i32 {\n    let s = \"hi\";\n    a\n}\n";
        for dark in [true, false] {
            let highlights = highlight_with(Some("rust"), text, dark, None).unwrap();
            let keyword = colored(&highlights, text, "fn").expect("keyword colored");
            let string = colored(&highlights, text, "\"hi\"").expect("string colored");
            let comment = colored(&highlights, text, "// add").expect("comment colored");
            assert_ne!(keyword, string);
            assert_ne!(keyword, comment);
            assert!(highlights.iter().all(|(range, _)| range.end <= text.len()));
        }
        assert!(highlight_with(Some("text"), text, true, None).is_none());
        assert!(highlight_with(None, text, true, None).is_none());
    }

    #[test]
    fn a_growing_block_resumes_with_the_same_result() {
        let full = "def f(x):\n    return \"\"\"doc\nstring\"\"\" + x  # tail\n";
        let mut incremental = None;
        for end in 1..=full.len() {
            if full.is_char_boundary(end) {
                incremental = highlight_with(Some("python"), &full[..end], true, None);
            }
        }
        let syntax = syntax_for("python").unwrap();
        let (fresh, _) = compute(syntax, true, full, None);
        assert_eq!(*incremental.unwrap(), *fresh);
    }

    #[test]
    fn long_blocks_are_highlighted_in_the_background() {
        let (redraw, mut redrawn) = futures::channel::mpsc::unbounded();
        let text = "package main\n\nfunc main() {\n\tprintln(\"background\")\n}\n";
        assert!(
            highlight_with(Some("go"), text, false, Some(&redraw)).is_none(),
            "shown plain at first"
        );
        assert!(
            highlight_with(Some("go"), &format!("{text}// more"), false, Some(&redraw)).is_none(),
            "a longer version waits for the job in flight"
        );
        futures::executor::block_on(redrawn.next()).expect("a redraw is requested");
        let ready = highlight_with(Some("go"), text, false, Some(&redraw)).expect("cached now");
        assert!(colored(&ready, text, "func").is_some());
        // The longer text resumes from the job's checkpoint, without a new job.
        assert!(
            highlight_with(Some("go"), &format!("{text}// more"), false, Some(&redraw)).is_some()
        );
        assert!(redrawn.try_recv().is_err());
    }
}
