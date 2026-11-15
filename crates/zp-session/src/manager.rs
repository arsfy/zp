//! Session files grouped by project directory.

use std::path::{Path, PathBuf};

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
}

impl SessionInfo {
    /// Summarizes an opened session.
    pub fn of(session: &Session, path: PathBuf) -> Self {
        let mut message_count = 0;
        let mut first_message = None;
        for entry in session.entries() {
            match entry.message() {
                Some(AgentMessage::Llm(Message::User(user))) => {
                    message_count += 1;
                    if first_message.is_none() {
                        let text: String = user
                            .content
                            .iter()
                            .filter_map(UserContent::as_text)
                            .collect();
                        first_message = Some(shorten(text.trim(), 200));
                    }
                }
                Some(AgentMessage::Llm(Message::Assistant(_))) => message_count += 1,
                _ => {}
            }
        }
        Self {
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
        }
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
#[derive(Clone, Debug)]
pub struct SessionManager {
    root: PathBuf,
}

impl SessionManager {
    /// Sessions stored under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
    pub fn list(&self, cwd: &Path) -> anyhow::Result<Vec<SessionInfo>> {
        list_dir(&self.project_dir(cwd))
    }

    /// Sessions of every project, most recently active first.
    pub fn list_all(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let mut all = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.root) else {
            return Ok(all);
        };
        for project in projects.flatten() {
            if project.file_type().is_ok_and(|t| t.is_dir()) {
                all.extend(list_dir(&project.path())?);
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
}

fn list_dir(dir: &Path) -> anyhow::Result<Vec<SessionInfo>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not list {}", dir.display()));
        }
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        match Session::open(&path) {
            Ok(session) => sessions.push(SessionInfo::of(&session, path)),
            Err(error) => tracing::warn!(path = %path.display(), %error, "skipping session"),
        }
    }
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.created.cmp(&a.created)));
    Ok(sessions)
}
