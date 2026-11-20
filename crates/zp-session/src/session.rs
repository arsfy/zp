//! One session: an append-only tree of entries, optionally backed by a JSONL file.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use zp_agent::{AgentMessage, CustomMessage};
use zp_ai::{Message, ThinkingLevel, UserContent, now_ms};

use crate::entry::{EntryData, HeaderType, SESSION_VERSION, SessionEntry, SessionHeader};

/// `custom_type` of the message that stands in for compacted history.
pub const COMPACTION_SUMMARY_TYPE: &str = "compaction_summary";

/// A model reference recorded in a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRef {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model_id: String,
}

/// What a branch of the session means for the agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionContext {
    /// Transcript, with compacted history replaced by its summary.
    pub messages: Vec<AgentMessage>,
    /// The entry each message came from (the compaction entry for its summary).
    pub entry_ids: Vec<String>,
    /// Last model used or selected on the branch.
    pub model: Option<ModelRef>,
    /// Last reasoning level selected on the branch.
    pub thinking_level: Option<ThinkingLevel>,
}

/// A node of [`Session::tree`].
#[derive(Clone, Debug, PartialEq)]
pub struct SessionTreeNode {
    /// The entry.
    pub entry: SessionEntry,
    /// Its label, if any.
    pub label: Option<String>,
    /// Children in creation order.
    pub children: Vec<SessionTreeNode>,
}

/// An append-only session tree.
///
/// Appends become children of the current leaf and advance it; [`Session::branch`] moves
/// the leaf to an earlier entry so the next append starts a new branch. A file-backed
/// session is written lazily: nothing touches the disk until the first user or assistant
/// message, so opening and closing without chatting leaves no file.
#[derive(Debug)]
pub struct Session {
    file: Option<PathBuf>,
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    index: HashMap<String, usize>,
    labels: HashMap<String, String>,
    leaf: Option<String>,
    flushed: bool,
    needs_newline: bool,
}

impl Session {
    /// A new session that will be written to `file` (if any) once it has a conversation.
    pub fn new(file: Option<PathBuf>, cwd: &Path, parent_session: Option<String>) -> Self {
        Self {
            file,
            header: SessionHeader {
                kind: HeaderType::Session,
                version: SESSION_VERSION,
                id: uuid::Uuid::now_v7().to_string(),
                timestamp: now_ms(),
                cwd: cwd.to_string_lossy().into_owned(),
                parent_session,
            },
            entries: Vec::new(),
            index: HashMap::new(),
            labels: HashMap::new(),
            leaf: None,
            flushed: false,
            needs_newline: false,
        }
    }

    /// A session that is never written to disk.
    pub fn in_memory(cwd: &Path) -> Self {
        Self::new(None, cwd, None)
    }

    /// Loads a session file. Malformed lines are skipped; the leaf is the last entry.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read session {}", path.display()))?;
        let mut lines = text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty());
        let header: SessionHeader = match lines.next() {
            Some((_, line)) => serde_json::from_str(line)
                .with_context(|| format!("{} is not a ZP session file", path.display()))?,
            None => bail!("{} is empty", path.display()),
        };
        if header.version > SESSION_VERSION {
            bail!(
                "{} was written by a newer ZP (format {}, this build supports {SESSION_VERSION})",
                path.display(),
                header.version
            );
        }
        let mut session = Self {
            file: Some(path.to_path_buf()),
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            labels: HashMap::new(),
            leaf: None,
            flushed: true,
            needs_newline: !text.is_empty() && !text.ends_with('\n'),
        };
        for (number, line) in lines {
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => session.insert(entry),
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    line = number + 1,
                    %error,
                    "skipping unreadable session entry"
                ),
            }
        }
        Ok(session)
    }

    fn insert(&mut self, entry: SessionEntry) {
        if let EntryData::Label { target_id, label } = &entry.data {
            match label {
                Some(label) => self.labels.insert(target_id.clone(), label.clone()),
                None => self.labels.remove(target_id),
            };
        }
        self.index.insert(entry.id.clone(), self.entries.len());
        self.leaf = Some(entry.id.clone());
        self.entries.push(entry);
    }

    // ----- accessors ---------------------------------------------------------------

    /// Session id.
    pub fn id(&self) -> &str {
        &self.header.id
    }

    /// Backing file, if any (it may not exist yet).
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Whether the backing file has been written.
    pub fn is_persisted(&self) -> bool {
        self.flushed && self.file.is_some()
    }

    /// The header.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Project directory.
    pub fn cwd(&self) -> PathBuf {
        PathBuf::from(&self.header.cwd)
    }

    /// All entries in file order.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// The entry `id`.
    pub fn entry(&self, id: &str) -> Option<&SessionEntry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// The current leaf id (`None` before the first entry or after [`Self::reset_leaf`]).
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// Direct children of `id`, in creation order.
    pub fn children(&self, id: &str) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.parent_id.as_deref() == Some(id))
            .collect()
    }

    /// The label of entry `id`.
    pub fn label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }

    /// The display name from the latest `session_info` entry.
    pub fn name(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.data {
                EntryData::SessionInfo { name } => Some(name.as_deref()),
                _ => None,
            })?
    }

    // ----- appending ---------------------------------------------------------------

    /// Appends `data` as a child of the leaf and makes it the leaf. Returns the entry id.
    /// If writing fails the entry is not added.
    pub fn append(&mut self, data: EntryData) -> anyhow::Result<String> {
        let entry = SessionEntry {
            id: self.new_entry_id(),
            parent_id: self.leaf.clone(),
            timestamp: now_ms(),
            data,
        };
        let id = entry.id.clone();
        let previous_leaf = self.leaf.clone();
        let previous_label = match &entry.data {
            EntryData::Label { target_id, .. } => Some(self.labels.get(target_id).cloned()),
            _ => None,
        };
        self.insert(entry);
        if let Err(error) = self.persist_last() {
            let entry = self.entries.pop().expect("just inserted");
            self.index.remove(&entry.id);
            self.leaf = previous_leaf;
            if let (EntryData::Label { target_id, .. }, Some(previous)) =
                (&entry.data, previous_label)
            {
                match previous {
                    Some(label) => self.labels.insert(target_id.clone(), label),
                    None => self.labels.remove(target_id),
                };
            }
            return Err(error);
        }
        Ok(id)
    }

    /// Appends a transcript message.
    pub fn append_message(&mut self, message: AgentMessage) -> anyhow::Result<String> {
        self.append(EntryData::Message {
            message,
            elapsed_seconds: None,
        })
    }

    /// Records a model switch.
    pub fn append_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> anyhow::Result<String> {
        self.append(EntryData::ModelChange {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        })
    }

    /// Records a reasoning level change.
    pub fn append_thinking_level_change(&mut self, level: ThinkingLevel) -> anyhow::Result<String> {
        self.append(EntryData::ThinkingLevelChange {
            thinking_level: level,
        })
    }

    /// Sets the display name.
    pub fn append_session_info(&mut self, name: Option<String>) -> anyhow::Result<String> {
        self.append(EntryData::SessionInfo { name })
    }

    /// Sets or clears the label of `target_id`.
    pub fn append_label(
        &mut self,
        target_id: &str,
        label: Option<String>,
    ) -> anyhow::Result<String> {
        if !self.index.contains_key(target_id) {
            bail!("entry {target_id} not found");
        }
        self.append(EntryData::Label {
            target_id: target_id.to_owned(),
            label: label.filter(|l| !l.trim().is_empty()),
        })
    }

    /// Records a compaction: history before `first_kept_entry_id` is replaced by `summary`.
    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: &str,
        tokens_before: u64,
        details: Option<Value>,
    ) -> anyhow::Result<String> {
        if !self.index.contains_key(first_kept_entry_id) {
            bail!("entry {first_kept_entry_id} not found");
        }
        self.append(EntryData::Compaction {
            summary,
            first_kept_entry_id: first_kept_entry_id.to_owned(),
            tokens_before,
            details,
        })
    }

    fn new_entry_id(&self) -> String {
        loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
            if !self.index.contains_key(&id) {
                return id;
            }
        }
    }

    fn has_conversation(&self) -> bool {
        self.entries.iter().any(|entry| {
            matches!(
                entry.message(),
                Some(AgentMessage::Llm(Message::User(_) | Message::Assistant(_)))
            )
        })
    }

    fn persist_last(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.file.clone() else {
            return Ok(());
        };
        if !self.flushed {
            if !self.has_conversation() {
                return Ok(());
            }
            return self.write_all(&path);
        }
        let entry = self.entries.last().expect("an entry was appended");
        let mut line = String::new();
        if self.needs_newline {
            line.push('\n');
        }
        line.push_str(&serde_json::to_string(entry)?);
        line.push('\n');
        OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .with_context(|| format!("could not append to {}", path.display()))?;
        self.needs_newline = false;
        Ok(())
    }

    /// Writes the header and every entry to a new file.
    fn write_all(&mut self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let mut text = serde_json::to_string(&self.header)?;
        text.push('\n');
        for entry in &self.entries {
            text.push_str(&serde_json::to_string(entry)?);
            text.push('\n');
        }
        let mut file = File::create_new(path)
            .with_context(|| format!("could not create {}", path.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("could not write {}", path.display()))?;
        self.flushed = true;
        Ok(())
    }

    // ----- branches ----------------------------------------------------------------

    /// Moves the leaf to `id`; the next append starts a new branch there.
    pub fn branch(&mut self, id: &str) -> anyhow::Result<()> {
        if !self.index.contains_key(id) {
            bail!("entry {id} not found");
        }
        self.leaf = Some(id.to_owned());
        Ok(())
    }

    /// Moves the leaf before the first entry; the next append starts a new root.
    pub fn reset_leaf(&mut self) {
        self.leaf = None;
    }

    /// Entries from the root to `leaf` (the current leaf when `None`).
    pub fn branch_path(&self, leaf: Option<&str>) -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut current = leaf.or(self.leaf.as_deref()).and_then(|id| self.entry(id));
        while let Some(entry) = current {
            path.push(entry);
            current = entry.parent_id.as_deref().and_then(|id| self.entry(id));
            if path.len() > self.entries.len() {
                tracing::error!("cycle in session tree");
                break;
            }
        }
        path.reverse();
        path
    }

    /// The most recent entry in the subtree of `id` (`id` itself if it has no
    /// descendants): where "switching to that branch" should land.
    pub fn latest_leaf_under(&self, id: &str) -> Option<&str> {
        let target = self.entry(id)?;
        for entry in self.entries.iter().rev() {
            let mut current = Some(entry);
            let mut steps = 0;
            while let Some(node) = current {
                if node.id == target.id {
                    return Some(&entry.id);
                }
                steps += 1;
                if steps > self.entries.len() {
                    break;
                }
                current = node.parent_id.as_deref().and_then(|p| self.entry(p));
            }
        }
        Some(&target.id)
    }

    /// The agent context of the current branch.
    pub fn context(&self) -> SessionContext {
        self.context_at(None)
    }

    /// The agent context of the branch ending at `leaf`.
    pub fn context_at(&self, leaf: Option<&str>) -> SessionContext {
        let path = self.branch_path(leaf);
        let mut context = SessionContext::default();
        for entry in &path {
            match &entry.data {
                EntryData::ModelChange { provider, model_id } => {
                    context.model = Some(ModelRef {
                        provider: provider.clone(),
                        model_id: model_id.clone(),
                    });
                }
                EntryData::ThinkingLevelChange { thinking_level } => {
                    context.thinking_level = Some(*thinking_level);
                }
                EntryData::Message {
                    message: AgentMessage::Llm(Message::Assistant(message)),
                    ..
                } => {
                    context.model = Some(ModelRef {
                        provider: message.provider.clone(),
                        model_id: message.model.clone(),
                    });
                }
                _ => {}
            }
        }

        let compaction = path
            .iter()
            .rposition(|entry| matches!(entry.data, EntryData::Compaction { .. }));
        let push = |entry: &SessionEntry, context: &mut SessionContext| {
            if let Some(message) = entry.message() {
                context.messages.push(message.clone());
                context.entry_ids.push(entry.id.clone());
            }
        };
        let Some(at) = compaction else {
            for entry in &path {
                push(entry, &mut context);
            }
            return context;
        };
        let EntryData::Compaction {
            summary,
            first_kept_entry_id,
            tokens_before,
            ..
        } = &path[at].data
        else {
            unreachable!()
        };
        let kept_from = path[..at]
            .iter()
            .position(|entry| &entry.id == first_kept_entry_id)
            .unwrap_or(at);
        let retained = path[kept_from..at]
            .iter()
            .filter(|entry| entry.message().is_some())
            .count();
        context.messages.push(
            CustomMessage {
                content: vec![UserContent::text(summary.clone())],
                details: Some(json!({"tokensBefore": tokens_before, "retainedMessages": retained})),
                timestamp: path[at].timestamp,
                ..CustomMessage::text(COMPACTION_SUMMARY_TYPE, "")
            }
            .into(),
        );
        context.entry_ids.push(path[at].id.clone());
        for entry in path[kept_from..at].iter().chain(&path[at + 1..]) {
            push(entry, &mut context);
        }
        context
    }

    /// The whole tree; roots and children in creation order.
    pub fn tree(&self) -> Vec<SessionTreeNode> {
        let mut children: HashMap<Option<&str>, Vec<&SessionEntry>> = HashMap::new();
        for entry in &self.entries {
            // Orphans (missing parent) are shown as roots.
            let parent = entry
                .parent_id
                .as_deref()
                .filter(|id| self.index.contains_key(*id));
            children.entry(parent).or_default().push(entry);
        }
        fn build(
            session: &Session,
            children: &HashMap<Option<&str>, Vec<&SessionEntry>>,
            parent: Option<&str>,
            depth: usize,
        ) -> Vec<SessionTreeNode> {
            if depth > session.entries.len() {
                return Vec::new();
            }
            children
                .get(&parent)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| SessionTreeNode {
                            entry: (*entry).clone(),
                            label: session.label(&entry.id).map(str::to_owned),
                            children: build(session, children, Some(&entry.id), depth + 1),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        build(self, &children, None, 0)
    }

    /// A new session holding the branch ending at `leaf_id`, written to `file` (if any)
    /// with this session as its parent.
    pub fn fork(&self, leaf_id: &str, file: Option<PathBuf>) -> anyhow::Result<Session> {
        if !self.index.contains_key(leaf_id) {
            bail!("entry {leaf_id} not found");
        }
        let parent = self
            .file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let mut forked = Session::new(None, &self.cwd(), parent);
        for entry in self.branch_path(Some(leaf_id)) {
            forked.insert(entry.clone());
        }
        if let Some(file) = file {
            forked.attach_file(file)?;
        }
        Ok(forked)
    }

    /// Backs an unwritten session with `file`, writing it right away if it already has a
    /// conversation.
    pub(crate) fn attach_file(&mut self, file: PathBuf) -> anyhow::Result<()> {
        debug_assert!(!self.flushed);
        if self.has_conversation() {
            self.write_all(&file)?;
        }
        self.file = Some(file);
        Ok(())
    }
}
