//! Session files grouped by project directory.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use anyhow::Context as _;
use zp_agent::AgentMessage;
use zp_ai::{Message, UserContent};

use crate::session::Session;
use crate::time::format_for_file_name;

/// Summary of a session file for listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// File path.
    pub path: PathBuf,
    /// Session id.
    pub id: String,
    /// Project directory.
    pub cwd: String,
    /// Display name, if set.
    pub name: Option<String>,
    /// Parent session file, for forks.
    pub parent_session: Option<String>,
    /// Creation time (Unix milliseconds).
    pub created: i64,
    /// Time of the last entry (Unix milliseconds).
    pub modified: i64,
    /// Number of user and assistant messages.
    pub message_count: usize,
    /// Text of the first user message (shortened).
    pub first_message: String,
    /// The title, then user and assistant text of the whole session (capped), lowercased
    /// once here: the haystack of [`Self::matches`].
    search_text: Arc<str>,
}

impl SessionInfo {
    /// Summarizes an opened session.
    pub fn of(session: &Session, path: PathBuf) -> Self {
        const SEARCH_TEXT_LIMIT: usize = 64 * 1024;
        let mut message_count = 0;
        let mut first_message = None;
        let mut search_text = String::new();
        let mut add_text = |text: &str| push_capped(&mut search_text, text, SEARCH_TEXT_LIMIT);
        for entry in session.entries() {
            match entry.message() {
                Some(AgentMessage::Llm(Message::User(user))) => {
                    message_count += 1;
                    let text: String = user
                        .content
                        .iter()
                        .filter_map(UserContent::as_text)
                        .collect();
                    if first_message.is_none() {
                        first_message = Some(shorten(text.trim(), 200));
                    }
                    add_text(&text);
                }
                Some(AgentMessage::Llm(Message::Assistant(assistant))) => {
                    message_count += 1;
                    add_text(&assistant.text());
                }
                _ => {}
            }
        }
        if let Some(name) = session.name() {
            add_text(name);
        }
        let mut info = Self {
            path,
            id: session.id().to_owned(),
            cwd: session.header().cwd.clone(),
            name: session.name().map(str::to_owned),
            parent_session: session.header().parent_session.clone(),
            created: session.header().timestamp,
            modified: session
                .entries()
                .last()
                .map_or(session.header().timestamp, |entry| entry.timestamp),
            message_count,
            first_message: first_message.unwrap_or_default(),
            search_text: Arc::from(""),
        };
        info.search_text = format!("{}\n{}", info.title(), search_text)
            .to_lowercase()
            .into();
        info
    }

    /// Whether every whitespace-separated word of `query` occurs (case-insensitively) in
    /// the title or text.
    pub fn matches(&self, query: &str) -> bool {
        if query.trim().is_empty() {
            return true;
        }
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| self.search_text.contains(word))
    }

    /// The name, else the first message, else `(empty)`.
    pub fn title(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.is_empty())
            .or(Some(self.first_message.as_str()).filter(|m| !m.is_empty()))
            .unwrap_or("(empty)")
    }
}

/// Appends `piece` and a newline to `text` without letting it grow past `limit` bytes,
/// cutting `piece` on a character boundary.
fn push_capped(text: &mut String, piece: &str, limit: usize) {
    let room = limit.saturating_sub(text.len());
    if room == 0 {
        return;
    }
    let mut cut = piece.len().min(room);
    while !piece.is_char_boundary(cut) {
        cut -= 1;
    }
    text.push_str(&piece[..cut]);
    if text.len() < limit {
        text.push('\n');
    }
}

fn shorten(text: &str, max_chars: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match single_line.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &single_line[..cut]),
        None => single_line,
    }
}

/// The directory name for a project: its path with separators replaced, e.g.
/// `/work/my-app` → `--work-my-app--`.
pub fn encode_cwd(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let trimmed = text.trim_start_matches(['/', '\\']);
    let safe: String = trimmed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{safe}--")
}

/// Creates, opens and lists sessions under a root directory (`~/.zp/sessions`), with one
/// subdirectory per project.
///
/// Listings remember the [`SessionInfo`] of each file and parse it again only once the
/// file changed; clones share what they remember.
#[derive(Clone, Debug)]
pub struct SessionManager {
    root: PathBuf,
    listed: Arc<Mutex<HashMap<PathBuf, ListedFile>>>,
}

/// A session file as last listed.
#[derive(Debug)]
struct ListedFile {
    stamp: FileStamp,
    info: SessionInfo,
}

/// The state of a session file that tells whether it changed: session files are only
/// appended to or rewritten, which shows in the length or modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    /// The current state of `path`; `None` if it cannot be read.
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

impl SessionManager {
    /// Sessions stored under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            listed: Arc::default(),
        }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding the sessions of `cwd`.
    pub fn project_dir(&self, cwd: &Path) -> PathBuf {
        self.root.join(encode_cwd(cwd))
    }

    fn new_file(&self, cwd: &Path, session: &Session) -> PathBuf {
        self.project_dir(cwd).join(format!(
            "{}_{}.jsonl",
            format_for_file_name(session.header().timestamp),
            session.id()
        ))
    }

    /// A new session for `cwd` (written on its first message).
    pub fn create(&self, cwd: &Path) -> Session {
        let mut session = Session::new(None, cwd, None);
        let file = self.new_file(cwd, &session);
        // A session without messages is not written, so this cannot fail.
        let _ = session.attach_file(file);
        session
    }

    /// Opens a session file.
    pub fn open(&self, path: &Path) -> anyhow::Result<Session> {
        Session::open(path)
    }

    /// Sessions of `cwd`, most recently active first. Unreadable files are skipped.
    ///
    /// Different directories can share a project directory (`/a-b` and `/a/b` both
    /// encode to `--a-b--`), so only sessions whose header names `cwd` are listed.
    pub fn list(&self, cwd: &Path) -> anyhow::Result<Vec<SessionInfo>> {
        let mut sessions = self.list_dir(&self.project_dir(cwd))?;
        sessions.retain(|info| Path::new(&info.cwd) == cwd);
        Ok(sessions)
    }

    /// Sessions of every project, most recently active first.
    pub fn list_all(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let mut all = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.root) else {
            return Ok(all);
        };
        for project in projects.flatten() {
            if project.file_type().is_ok_and(|t| t.is_dir()) {
                all.extend(self.list_dir(&project.path())?);
            }
        }
        all.sort_by_key(|info| std::cmp::Reverse(info.modified));
        Ok(all)
    }

    /// The most recently active session of `cwd`.
    pub fn most_recent(&self, cwd: &Path) -> anyhow::Result<Option<SessionInfo>> {
        Ok(self.list(cwd)?.into_iter().next())
    }

    /// Opens the most recent session of `cwd`, or creates one.
    pub fn continue_recent(&self, cwd: &Path) -> anyhow::Result<Session> {
        match self.most_recent(cwd)? {
            Some(info) => self.open(&info.path),
            None => Ok(self.create(cwd)),
        }
    }

    /// Finds a session of `cwd` by id prefix (or file path).
    pub fn find(&self, cwd: &Path, id_or_path: &str) -> anyhow::Result<Option<SessionInfo>> {
        let as_path = Path::new(id_or_path);
        if as_path.is_file() {
            let session = self.open(as_path)?;
            return Ok(Some(SessionInfo::of(&session, as_path.to_path_buf())));
        }
        let matches: Vec<SessionInfo> = self
            .list(cwd)?
            .into_iter()
            .filter(|info| info.id.starts_with(id_or_path))
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            n => anyhow::bail!("{n} sessions match `{id_or_path}`; use a longer prefix"),
        }
    }

    /// Copies the branch of `session` ending at `leaf_id` into a new session file of the
    /// same project.
    pub fn fork(&self, session: &Session, leaf_id: &str) -> anyhow::Result<Session> {
        let cwd = session.cwd();
        let mut forked = session.fork(leaf_id, None)?;
        let file = self.new_file(&cwd, &forked);
        forked.attach_file(file)?;
        Ok(forked)
    }

    /// Sessions in project directory `dir`, most recently active first.
    fn list_dir(&self, dir: &Path) -> anyhow::Result<Vec<SessionInfo>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.forget_listed_in(dir, &HashSet::new());
                return Ok(Vec::new());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not list {}", dir.display()));
            }
        };
        let mut sessions = Vec::new();
        let mut present = HashSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Some(info) = self.listed_info(&path) {
                sessions.push(info);
            }
            present.insert(path);
        }
        self.forget_listed_in(dir, &present);
        sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.created.cmp(&a.created)));
        Ok(sessions)
    }

    /// The summary of session file `path`, parsed again only if the file changed since it
    /// was last listed. `None` (with a warning) for unreadable files.
    fn listed_info(&self, path: &Path) -> Option<SessionInfo> {
        // Taken before reading, so a write in between makes the next listing read again.
        let stamp = FileStamp::of(path);
        if let Some(stamp) = stamp
            && let Some(listed) = self.listed().get(path)
            && listed.stamp == stamp
        {
            return Some(listed.info.clone());
        }
        match Session::open(path) {
            Ok(session) => {
                let info = SessionInfo::of(&session, path.to_path_buf());
                if let Some(stamp) = stamp {
                    let listed = ListedFile {
                        stamp,
                        info: info.clone(),
                    };
                    self.listed().insert(path.to_path_buf(), listed);
                }
                Some(info)
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "skipping session");
                self.listed().remove(path);
                None
            }
        }
    }

    /// Forgets the listed files of `dir` other than `present`.
    fn forget_listed_in(&self, dir: &Path, present: &HashSet<PathBuf>) {
        self.listed()
            .retain(|path, _| path.parent() != Some(dir) || present.contains(path));
    }

    /// The remembered files (still usable after a panic elsewhere held the lock).
    fn listed(&self) -> MutexGuard<'_, HashMap<PathBuf, ListedFile>> {
        self.listed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(manager: &SessionManager, cwd: &Path) -> Vec<String> {
        let sessions = manager.list(cwd).unwrap();
        sessions
            .iter()
            .map(|info| info.title().to_owned())
            .collect()
    }

    #[test]
    fn listing_parses_only_changed_files_again() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::new(root.path());
        let cwd = Path::new("/p");
        let mut session = manager.create(cwd);
        session
            .append_message(AgentMessage::user("Fix the parser"))
            .unwrap();
        let path = session.file().unwrap().to_path_buf();
        assert_eq!(titles(&manager, cwd), ["Fix the parser"]);

        // Same length and modification time: the remembered summary is used.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("Fix the parser", "Fix the lexer!")).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);
        assert_eq!(titles(&manager, cwd), ["Fix the parser"]);
        assert_eq!(manager.clone().list(cwd).unwrap().len(), 1);

        // A changed file is read again.
        let mut reopened = Session::open(&path).unwrap();
        reopened
            .append_message(AgentMessage::user("and the lexer"))
            .unwrap();
        assert_eq!(titles(&manager, cwd), ["Fix the lexer!"]);

        // A deleted file is forgotten.
        std::fs::remove_file(&path).unwrap();
        assert!(titles(&manager, cwd).is_empty());
        assert!(manager.listed().is_empty());
    }

    #[test]
    fn search_text_is_capped_on_a_char_boundary() {
        let mut session = Session::in_memory(Path::new("/p"));
        let long = format!("{} needle", "é".repeat(40 * 1024));
        session.append_message(AgentMessage::user(&long)).unwrap();
        session
            .append_message(AgentMessage::user("haystack"))
            .unwrap();
        let info = SessionInfo::of(&session, PathBuf::from("s.jsonl"));
        assert!(info.matches("ééé"));
        assert!(!info.matches("needle"), "text past the cap is not searched");
        assert!(!info.matches("haystack"));
        assert!(info.search_text.len() <= info.title().len() + 1 + 64 * 1024);
    }
}
