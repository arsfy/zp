//! `@file` references and image attachments of a prompt.
//!
//! `@path` in a message attaches that file: text files are included as
//! `<file name="…">…</file>` blocks before the message (pi's format), images as image
//! content. Pasted or dropped images are attached directly.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use zp_agent::AgentMessage;
use zp_ai::{ImageContent, UserContent, UserMessage, now_ms};

/// Largest text file included by reference.
pub const MAX_TEXT_BYTES: u64 = 256 * 1024;
/// Largest image attached.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// An image to send with a prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAttachment {
    /// Shown in the composer (file name or "Pasted image").
    pub name: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// The image MIME type of `path`, by extension.
pub fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

impl ImageAttachment {
    /// Reads an image file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let mime = image_mime(path).ok_or_else(|| {
            anyhow::anyhow!("{} is not a PNG, JPEG, GIF or WebP image", path.display())
        })?;
        let data = std::fs::read(path)?;
        anyhow::ensure!(
            data.len() <= MAX_IMAGE_BYTES,
            "{} is larger than 5 MB",
            path.display()
        );
        Ok(Self {
            name: path
                .file_name()
                .map_or_else(|| "image".to_owned(), |n| n.to_string_lossy().into_owned()),
            mime_type: mime.to_owned(),
            data,
        })
    }

    fn content(&self) -> UserContent {
        UserContent::Image(ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&self.data),
            mime_type: self.mime_type.clone(),
        })
    }
}

/// The `@path` references in `text`: `@` at the start or after whitespace, up to the next
/// whitespace, without trailing punctuation; or `@"a path"` in quotes, which may contain
/// spaces.
pub fn references(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut previous = ' ';
    for (index, c) in text.char_indices() {
        if c == '@' && previous.is_whitespace() {
            let rest = &text[index + 1..];
            let path = match rest.strip_prefix('"') {
                Some(quoted) => quoted.find('"').map_or("", |end| &quoted[..end]),
                None => {
                    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    rest[..end].trim_end_matches([',', '.', ';', ':', ')', '!', '?', '"', '\''])
                }
            };
            if !path.is_empty() && !refs.iter().any(|r| r == path) {
                refs.push(path.to_owned());
            }
        }
        previous = c;
    }
    refs
}

/// `@path` as typed: quoted when it contains whitespace. A folder ends with `/` and, when
/// quoted, leaves the quote open so its contents can be completed next.
pub fn reference(path: &str, dir: bool) -> String {
    let path = if dir {
        format!("{}/", path.trim_end_matches('/'))
    } else {
        path.to_owned()
    };
    match (path.contains(char::is_whitespace), dir) {
        (false, _) => format!("@{path}"),
        (true, false) => format!("@\"{path}\""),
        (true, true) => format!("@\"{path}"),
    }
}

/// An `@path` being typed at the caret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceAt {
    /// Byte range of the whole reference, from `@` to its end (past the caret too).
    pub range: std::ops::Range<usize>,
    /// What follows `@` up to the caret, without an opening quote.
    pub query: String,
}

/// The `@path` reference the caret at byte `caret` is in, if any: `@` at the start or
/// after whitespace, then no whitespace up to the caret (or an open `@"` quote).
pub fn reference_at(text: &str, caret: usize) -> Option<ReferenceAt> {
    let caret = caret.min(text.len());
    let before = text.get(..caret)?;
    for (at, _) in before
        .match_indices('@')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if before[..at]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
        {
            continue;
        }
        let typed = &before[at + 1..];
        let after = &text[caret..];
        return match typed.strip_prefix('"') {
            // A closed quote ends the reference before the caret.
            Some(quoted) if quoted.contains('"') => None,
            Some(quoted) => Some(ReferenceAt {
                range: at..caret + after.find('"').map_or(after.len(), |end| end + 1),
                query: quoted.to_owned(),
            }),
            None if typed.contains(char::is_whitespace) => None,
            None => Some(ReferenceAt {
                range: at..caret + after.find(char::is_whitespace).unwrap_or(after.len()),
                query: typed.to_owned(),
            }),
        };
    }
    None
}

/// The user message for `text` with its `@file` references and `images` attached.
/// Returns the message and notes about references that could not be attached.
pub fn build_message(
    text: &str,
    cwd: &Path,
    images: &[ImageAttachment],
) -> (AgentMessage, Vec<String>) {
    let mut notes = Vec::new();
    let mut blocks = String::new();
    let mut attached: Vec<ImageAttachment> = Vec::new();
    for reference in references(text) {
        let path = zp_tools::resolve_path(cwd, &reference);
        if !path.is_file() {
            continue;
        }
        let shown = zp_tools::display_path(cwd, &path);
        if image_mime(&path).is_some() {
            match ImageAttachment::from_file(&path) {
                Ok(image) => {
                    blocks.push_str(&format!("<file name=\"{shown}\"></file>\n"));
                    attached.push(image);
                }
                Err(error) => notes.push(format!("{error:#}")),
            }
            continue;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_TEXT_BYTES {
            notes.push(format!(
                "{shown} is larger than 256 KB; the agent can read it with its tools"
            ));
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => blocks.push_str(&format!(
                "<file name=\"{shown}\">\n{}\n</file>\n",
                content.trim_start_matches('\u{FEFF}').trim_end()
            )),
            Err(_) => notes.push(format!("{shown} is not a text file")),
        }
    }
    let mut content = vec![UserContent::text(format!("{blocks}{text}"))];
    content.extend(images.iter().chain(&attached).map(ImageAttachment::content));
    let message = AgentMessage::Llm(zp_ai::Message::User(UserMessage {
        content,
        timestamp: now_ms(),
    }));
    (message, notes)
}

/// Splits the leading `<file name="…">…</file>` blocks [`build_message`] added from the
/// typed text: returns the file names and the text.
pub fn split_file_blocks(text: &str) -> (Vec<String>, &str) {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(after) = rest.strip_prefix("<file name=\"") {
        let Some(quote) = after.find('"') else { break };
        let name = &after[..quote];
        let Some(end) = after.find("</file>\n") else {
            break;
        };
        names.push(name.to_owned());
        rest = &after[end + "</file>\n".len()..];
    }
    (names, rest)
}

/// A file or folder offered for `@` completion (a project-relative path with `/`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
    pub path: String,
    pub dir: bool,
}

/// The project's files and the folders that contain them, sorted (`.gitignore` respected).
pub fn project_entries(cwd: &Path, limit: usize) -> Vec<PathEntry> {
    let files = project_files(cwd, limit);
    let mut dirs = std::collections::BTreeSet::new();
    for file in &files {
        let mut path = file.as_str();
        while let Some((parent, _)) = path.rsplit_once('/') {
            if !dirs.insert(parent.to_owned()) {
                break;
            }
            path = parent;
        }
    }
    let mut entries: Vec<PathEntry> = dirs
        .into_iter()
        .map(|path| PathEntry { path, dir: true })
        .chain(files.into_iter().map(|path| PathEntry { path, dir: false }))
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Completions for `query`, what follows `@`. An empty query, or one ending in `/`, lists
/// what that folder contains (folders first); otherwise (or when there is no such folder)
/// the best fuzzy matches, paths starting with the query first.
pub fn complete_path<'a>(
    entries: &'a [PathEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a PathEntry> {
    if query.is_empty() || query.ends_with('/') {
        let prefix = query.to_lowercase();
        let mut children: Vec<&PathEntry> = entries
            .iter()
            .filter(|entry| {
                let lower = entry.path.to_lowercase();
                lower
                    .strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .collect();
        children.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.path.cmp(&b.path)));
        children.truncate(limit);
        if !children.is_empty() || query.is_empty() {
            return children;
        }
    }
    let lower = query.to_lowercase();
    let mut scored: Vec<(i64, &PathEntry)> = entries
        .iter()
        .filter_map(|entry| {
            let shown = if entry.dir {
                format!("{}/", entry.path)
            } else {
                entry.path.clone()
            };
            let mut score = fuzzy_score(&shown, query)?;
            if shown.to_lowercase().starts_with(&lower) {
                score += 1_000_000;
            }
            Some((score, entry))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.path.cmp(&b.1.path)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect()
}

/// Files of the project for `@` completion (relative paths, `.gitignore` respected).
pub fn project_files(cwd: &Path, limit: usize) -> Vec<String> {
    let mut files: Vec<String> = ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| {
            let path: PathBuf = entry.into_path();
            let relative = path.strip_prefix(cwd).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .take(limit)
        .collect();
    files.sort();
    files
}

/// How well `candidate` matches `query` (its characters in order): higher is better;
/// `None` when it does not match. Matches in the file name and contiguous runs count more.
pub fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let lower = candidate.to_lowercase();
    let name_start = lower.rfind('/').map_or(0, |i| i + 1);
    let mut score = 0i64;
    let mut position = 0;
    let mut previous: Option<usize> = None;
    for q in query.to_lowercase().chars() {
        let found = lower[position..].find(q)? + position;
        score += 1;
        if previous.is_some_and(|p| p + 1 == found) {
            score += 5;
        }
        if found >= name_start {
            score += 3;
        }
        if found == name_start || lower[..found].ends_with(['/', '_', '-', '.']) {
            score += 4;
        }
        previous = Some(found);
        position = found + q.len_utf8();
    }
    // Shorter paths first among equals.
    Some(score * 1000 - candidate.len() as i64)
}

/// The best `limit` matches of `query` among `files`.
pub fn fuzzy_filter<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i64, &str)> = files
        .iter()
        .filter_map(|f| Some((fuzzy_score(f, query)?, f.as_str())))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, f)| f).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_references_may_contain_spaces() {
        assert_eq!(
            references(r#"see @"docs/my notes.md" and @"unclosed path"#),
            ["docs/my notes.md"]
        );
        assert_eq!(
            reference("docs/my notes.md", false),
            r#"@"docs/my notes.md""#
        );
        assert_eq!(reference("my docs", true), r#"@"my docs/"#);
        assert_eq!(reference("src", true), "@src/");
        assert_eq!(reference("src/main.rs", false), "@src/main.rs");
    }

    #[test]
    fn finds_the_reference_at_the_caret() {
        let text = "fix @src/ma and email@host";
        // In the middle of a reference: it reaches past the caret to its end.
        assert_eq!(
            reference_at(text, 10),
            Some(ReferenceAt {
                range: 4..11,
                query: "src/m".into()
            })
        );
        assert_eq!(reference_at(text, 3), None, "before the @");
        assert_eq!(reference_at(text, 15), None, "after the reference");
        assert_eq!(reference_at(text, text.len()), None, "an email address");
        assert_eq!(
            reference_at("@", 1),
            Some(ReferenceAt {
                range: 0..1,
                query: String::new()
            })
        );
        let quoted = r#"open @"my do"#;
        assert_eq!(
            reference_at(quoted, quoted.len()).map(|r| r.query),
            Some("my do".into())
        );
        assert_eq!(reference_at(r#"open @"my doc" now"#, 18), None);
    }

    #[test]
    fn completes_folders_by_listing_and_names_by_fuzzy_match() {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "src/main.rs",
            "src/ui/view.rs",
            "README.md",
            "scripts/main.sh",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let entries = project_entries(dir.path(), 100);
        let shown = |query: &str| {
            complete_path(&entries, query, 10)
                .into_iter()
                .map(|entry| {
                    if entry.dir {
                        format!("{}/", entry.path)
                    } else {
                        entry.path.clone()
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(""), ["scripts/", "src/", "README.md"]);
        assert_eq!(shown("src/"), ["src/ui/", "src/main.rs"]);
        assert_eq!(shown("src/u"), ["src/ui/", "src/ui/view.rs"]);
        assert_eq!(shown("sr/")[0], "src/", "no such folder: a fuzzy match");
        assert_eq!(shown("main")[..2], ["src/main.rs", "scripts/main.sh"]);
    }

    #[test]
    fn finds_references() {
        assert_eq!(
            references("look at @src/main.rs, and @README.md. not email@host @src/main.rs"),
            ["src/main.rs", "README.md"]
        );
        assert!(references("@").is_empty());
    }

    #[test]
    fn builds_messages_with_files_and_images() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "\u{FEFF}fn a() {}\n").unwrap();
        std::fs::write(root.join("logo.png"), [0x89, b'P', b'N', b'G']).unwrap();
        std::fs::write(root.join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        let pasted = ImageAttachment {
            name: "Pasted image".into(),
            mime_type: "image/png".into(),
            data: vec![1, 2, 3],
        };
        let (message, notes) = build_message(
            "explain @src/lib.rs and @logo.png, ignore @missing.rs and @bin.dat",
            root,
            &[pasted],
        );
        let AgentMessage::Llm(zp_ai::Message::User(user)) = message else {
            panic!()
        };
        assert_eq!(
            user.content[0].as_text().unwrap(),
            "<file name=\"src/lib.rs\">\nfn a() {}\n</file>\n<file name=\"logo.png\"></file>\nexplain @src/lib.rs and @logo.png, ignore @missing.rs and @bin.dat"
        );
        assert_eq!(
            user.content.len(),
            3,
            "text, pasted image, referenced image"
        );
        let (names, rest) = split_file_blocks(user.content[0].as_text().unwrap());
        assert_eq!(names, ["src/lib.rs", "logo.png"]);
        assert!(rest.starts_with("explain @src/lib.rs"));
        assert_eq!(split_file_blocks("plain"), (vec![], "plain"));
        assert!(matches!(&user.content[1], UserContent::Image(i) if i.data == "AQID"));
        assert_eq!(notes, ["bin.dat is not a text file"]);
    }

    #[test]
    fn lists_and_ranks_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/conversation")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        for file in [
            "src/main.rs",
            "src/conversation/render.rs",
            "README.md",
            "target/out.rs",
        ] {
            std::fs::write(root.join(file), "").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        // `ignore` honors .gitignore inside git repositories.
        std::fs::create_dir(root.join(".git")).unwrap();
        let files = project_files(root, 100);
        assert_eq!(
            files,
            ["README.md", "src/conversation/render.rs", "src/main.rs"]
        );
        assert_eq!(fuzzy_filter(&files, "main", 5), ["src/main.rs"]);
        assert_eq!(
            fuzzy_filter(&files, "rend", 5)[0],
            "src/conversation/render.rs"
        );
        assert_eq!(fuzzy_filter(&files, "src", 5).len(), 2);
        assert!(fuzzy_filter(&files, "zzz", 5).is_empty());
    }
}
