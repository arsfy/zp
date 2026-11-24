//! The file changes along one branch of a session, grouped by turn or retry, and
//! what reverting them means.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zp_agent::{AgentMessage, CustomMessage};
use zp_ai::{Message, UserContent};
use zp_session::{EntryData, SessionEntry};

use crate::checkpoint::{BlobStore, ContentRef, RecordedChange};
use crate::restore::FileTarget;

/// `custom_type` of the transcript message recording a file restore.
pub const FILE_RESTORE_TYPE: &str = "zp.file_restore";

/// Why a group of changes exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupKind {
    /// A user message and the agent run that answered it.
    Turn {
        /// The user's text.
        prompt: String,
    },
    /// A run that retried a failed or aborted response.
    Retry {
        /// 1-based retry number within the turn.
        attempt: usize,
    },
    /// Files restored by the user (see [`FILE_RESTORE_TYPE`]).
    Restore {
        /// What was restored, e.g. "Reverted turn 2".
        description: String,
    },
}

/// One file change on the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRecord {
    /// The session entry holding it (a tool result or a restore message).
    pub entry_id: String,
    /// The tool call that made it (none for restores).
    pub tool_call_id: Option<String>,
    pub change: RecordedChange,
    /// Whether a later, still-active restore reverted this change.
    pub reverted: bool,
    /// Entry ids reverted by this restore, for this file.
    reverts: Vec<String>,
}

/// A recorded change targeted by a restore. Paths distinguish files in restore entries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreSource {
    pub entry_id: String,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RevertStatus {
    #[default]
    Active,
    Partial,
    Reverted,
}

impl RevertStatus {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Active => None,
            Self::Partial => Some("Partially reverted"),
            Self::Reverted => Some("Reverted"),
        }
    }
}

/// The changes made in one turn, retry, or restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeGroup {
    pub kind: GroupKind,
    /// The user message, retry, or restore entry. Changes before the first user message
    /// may have no starting entry.
    pub entry_id: Option<String>,
    /// The final assistant entry of this turn or retry on the current branch.
    pub response_entry_id: Option<String>,
    /// 1-based number among the turns of the branch (retries and restores share it).
    pub turn: usize,
    pub timestamp: i64,
    pub changes: Vec<ChangeRecord>,
}

impl ChangeGroup {
    /// A short label: `Turn 3: Fix the parser` / `Restore: …`.
    pub fn label(&self) -> String {
        match &self.kind {
            GroupKind::Turn { prompt } => {
                let line = prompt.lines().next().unwrap_or_default().trim();
                format!("Turn {}: {line}", self.turn)
            }
            GroupKind::Retry { attempt } => format!("Turn {} retry {}", self.turn, attempt),
            GroupKind::Restore { description } => description.clone(),
        }
    }
}

/// Which changes a summary or revert covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every change on the branch.
    Session,
    /// One group (index into [`ChangeLog::groups`]).
    Group(usize),
}

/// A file's net change within a scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSummary {
    pub path: PathBuf,
    /// Content before the first change in scope.
    pub start: ContentRef,
    /// Content after the last change in scope.
    pub end: ContentRef,
    /// Number of changes in scope.
    pub edits: usize,
    /// `(added, removed)` when known without loading content (a single change).
    pub stats: Option<(usize, usize)>,
}

impl FileSummary {
    /// Whether the changes in scope cancel out.
    pub fn is_unchanged(&self) -> bool {
        self.start.same_as(&self.end)
    }
}

/// The file changes along one branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeLog {
    pub groups: Vec<ChangeGroup>,
}

/// `details` of a restore message.
#[derive(Serialize, Deserialize)]
struct RestoreDetails {
    description: String,
    changes: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sources: Vec<RestoreSource>,
}

/// The transcript message recording that the user restored files to earlier versions.
/// It tells the model, which would otherwise assume its edits are still there.
pub fn restore_message(description: &str, changes: &[RecordedChange], cwd: &Path) -> CustomMessage {
    restore_message_with_sources(description, changes, &[], cwd)
}

pub fn restore_message_with_sources(
    description: &str,
    changes: &[RecordedChange],
    sources: &[RestoreSource],
    cwd: &Path,
) -> CustomMessage {
    let files: Vec<String> = changes
        .iter()
        .map(|change| {
            let shown = change
                .path
                .strip_prefix(cwd)
                .unwrap_or(&change.path)
                .display()
                .to_string();
            if change.is_deletion() {
                format!("{shown} (deleted)")
            } else {
                shown
            }
        })
        .collect();
    let text = format!(
        "[{description}] The user restored {} file(s) to an earlier version: {}. Read files again before editing them.",
        files.len(),
        files.join(", ")
    );
    let mut message = CustomMessage::text(FILE_RESTORE_TYPE, text);
    message.details = serde_json::to_value(RestoreDetails {
        description: description.to_owned(),
        changes: changes.iter().map(RecordedChange::to_details).collect(),
        sources: sources
            .iter()
            .filter(|source| changes.iter().any(|change| change.path == source.path))
            .cloned()
            .collect(),
    })
    .ok();
    message
}

/// A one-line summary of a restore message for display: `Reverted turn 2 — a.rs, b.rs`.
pub fn restore_summary(message: &CustomMessage) -> Option<String> {
    if message.custom_type != FILE_RESTORE_TYPE {
        return None;
    }
    let details: RestoreDetails = serde_json::from_value(message.details.clone()?).ok()?;
    let files: Vec<String> = details
        .changes
        .iter()
        .filter_map(RecordedChange::from_details)
        .map(|change| {
            let name = change.path.file_name().map_or_else(
                || change.path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            if change.is_deletion() {
                format!("{name} (deleted)")
            } else {
                name
            }
        })
        .collect();
    Some(format!("{} — {}", details.description, files.join(", ")))
}

impl ChangeLog {
    /// Collects the changes along a branch path (root first).
    pub fn from_path(path: &[&SessionEntry]) -> Self {
        let mut log = Self::default();
        let mut turns = 0;
        let mut retries = 0;
        let mut response_group = None;
        for entry in path {
            if matches!(entry.data, EntryData::Retry) {
                retries += 1;
                response_group = Some(log.groups.len());
                log.groups.push(ChangeGroup {
                    kind: GroupKind::Retry { attempt: retries },
                    entry_id: Some(entry.id.clone()),
                    response_entry_id: None,
                    turn: turns,
                    timestamp: entry.timestamp,
                    changes: Vec::new(),
                });
                continue;
            }
            let Some(message) = entry.message() else {
                continue;
            };
            match message {
                AgentMessage::Llm(Message::User(user)) => {
                    turns += 1;
                    retries = 0;
                    let prompt = user
                        .content
                        .iter()
                        .filter_map(UserContent::as_text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    response_group = Some(log.groups.len());
                    log.groups.push(ChangeGroup {
                        kind: GroupKind::Turn { prompt },
                        entry_id: Some(entry.id.clone()),
                        response_entry_id: None,
                        turn: turns,
                        timestamp: entry.timestamp,
                        changes: Vec::new(),
                    });
                }
                AgentMessage::Llm(Message::Assistant(_)) => {
                    if let Some(index) = response_group {
                        log.groups[index].response_entry_id = Some(entry.id.clone());
                    }
                }
                AgentMessage::Llm(Message::ToolResult(result)) if !result.is_error => {
                    let Some(change) = result
                        .details
                        .as_ref()
                        .and_then(RecordedChange::from_details)
                    else {
                        continue;
                    };
                    if log.groups.is_empty() {
                        log.groups.push(ChangeGroup {
                            kind: GroupKind::Turn {
                                prompt: String::new(),
                            },
                            entry_id: None,
                            response_entry_id: None,
                            turn: 0,
                            timestamp: entry.timestamp,
                            changes: Vec::new(),
                        });
                    }
                    let group = log.groups.last_mut().expect("a group exists");
                    group.changes.push(ChangeRecord {
                        entry_id: entry.id.clone(),
                        tool_call_id: Some(result.tool_call_id.clone()),
                        change,
                        reverted: false,
                        reverts: Vec::new(),
                    });
                }
                AgentMessage::Custom(custom) if custom.custom_type == FILE_RESTORE_TYPE => {
                    let details = match custom
                        .details
                        .clone()
                        .map(serde_json::from_value::<RestoreDetails>)
                    {
                        Some(Ok(details)) => details,
                        Some(Err(error)) => {
                            tracing::warn!(
                                entry = %entry.id,
                                %error,
                                "skipping unreadable file restore details"
                            );
                            continue;
                        }
                        None => continue,
                    };
                    let changes = details
                        .changes
                        .iter()
                        .filter_map(|details| {
                            let change = RecordedChange::from_details(details);
                            if change.is_none() {
                                tracing::warn!(
                                    entry = %entry.id,
                                    "skipping unreadable change of a file restore"
                                );
                            }
                            change
                        })
                        .map(|change| ChangeRecord {
                            entry_id: entry.id.clone(),
                            tool_call_id: None,
                            reverted: false,
                            reverts: details
                                .sources
                                .iter()
                                .filter(|source| source.path == change.path)
                                .map(|source| source.entry_id.clone())
                                .collect(),
                            change,
                        })
                        .collect();
                    log.groups.push(ChangeGroup {
                        kind: GroupKind::Restore {
                            description: details.description,
                        },
                        entry_id: Some(entry.id.clone()),
                        response_entry_id: None,
                        turn: turns,
                        timestamp: entry.timestamp,
                        changes,
                    });
                }
                _ => {}
            }
        }
        log.resolve_reverts(None);
        log
    }

    /// Rebuilds restore state from persisted sources, inferring older records from snapshots.
    pub fn resolve_reverts(&mut self, store: Option<&BlobStore>) {
        for index in 0..self.groups.len() {
            if !matches!(self.groups[index].kind, GroupKind::Restore { .. }) {
                continue;
            }
            for file_index in 0..self.groups[index].changes.len() {
                let record = &self.groups[index].changes[file_index];
                if !record.reverts.is_empty() {
                    continue;
                }
                let restored = &record.change;
                let mut sources = HashSet::new();
                for group in self.groups.iter().take(index) {
                    let records = group
                        .changes
                        .iter()
                        .filter(|record| record.change.path == restored.path)
                        .collect::<Vec<_>>();
                    let matches = |before: &ContentRef, after: &ContentRef| {
                        if before.same_as(after) {
                            return false;
                        }
                        if before.same_as(&restored.after) && after.same_as(&restored.before) {
                            return true;
                        }
                        let Some(store) = store else {
                            return false;
                        };
                        let load = || -> anyhow::Result<bool> {
                            let expected = after.load(store)?;
                            let target = before.load(store)?;
                            let current = restored.before.load(store)?;
                            let result = restored.after.load(store)?;
                            Ok(match (expected, target, current, result) {
                                (Some(expected), Some(target), Some(current), Some(result)) => {
                                    crate::restore::merge3(&expected, &current, &target)
                                        .is_some_and(|merged| merged == result.as_ref())
                                }
                                _ => false,
                            })
                        };
                        load().unwrap_or(false)
                    };
                    if let (Some(first), Some(last)) = (records.first(), records.last())
                        && matches(&first.change.before, &last.change.after)
                    {
                        sources.extend(records.iter().map(|record| record.entry_id.clone()));
                    } else {
                        sources.extend(
                            records
                                .iter()
                                .filter(|record| {
                                    matches(&record.change.before, &record.change.after)
                                })
                                .map(|record| record.entry_id.clone()),
                        );
                    }
                }
                let mut sources = sources.into_iter().collect::<Vec<_>>();
                sources.sort();
                self.groups[index].changes[file_index].reverts = sources;
            }
        }
        let mut reverted = HashSet::new();
        for record in self
            .groups
            .iter_mut()
            .rev()
            .flat_map(|group| group.changes.iter_mut().rev())
        {
            record.reverted =
                reverted.contains(&(record.entry_id.clone(), record.change.path.clone()));
            if !record.reverted {
                for source in &record.reverts {
                    reverted.insert((source.clone(), record.change.path.clone()));
                }
            }
        }
    }

    pub fn revert_sources(&self, scope: Scope, path: Option<&Path>) -> Vec<RestoreSource> {
        self.records(scope)
            .filter(|record| path.is_none_or(|path| path == record.change.path))
            .map(|record| RestoreSource {
                entry_id: record.entry_id.clone(),
                path: record.change.path.clone(),
            })
            .collect()
    }

    pub fn revert_status(&self, scope: Scope, path: Option<&Path>) -> RevertStatus {
        let mut active = false;
        let mut reverted = false;
        let changed_paths = self
            .files(scope)
            .into_iter()
            .filter(|file| !file.is_unchanged())
            .map(|file| file.path)
            .collect::<HashSet<_>>();
        for record in self
            .records(scope)
            .filter(|record| path.is_none_or(|path| path == record.change.path))
            .filter(|record| changed_paths.contains(&record.change.path))
            .filter(|record| !record.change.before.same_as(&record.change.after))
        {
            if record.reverted {
                reverted = true;
            } else {
                active = true;
            }
        }
        match (active, reverted) {
            (false, true) => RevertStatus::Reverted,
            (true, true) => RevertStatus::Partial,
            _ => RevertStatus::Active,
        }
    }

    pub fn tool_reverted(&self, call_id: &str) -> bool {
        self.records(Scope::Session)
            .find(|record| record.tool_call_id.as_deref() == Some(call_id))
            .is_some_and(|record| record.reverted)
    }

    /// Whether no file was changed.
    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|group| group.changes.is_empty())
    }

    /// Groups that changed files, with their indices, newest first.
    pub fn groups_with_changes(&self) -> impl Iterator<Item = (usize, &ChangeGroup)> {
        self.groups
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, group)| !group.changes.is_empty())
    }

    /// The group (index) containing the change made by `tool_call_id`.
    pub fn group_of_tool_call(&self, tool_call_id: &str) -> Option<usize> {
        self.groups.iter().position(|group| {
            group
                .changes
                .iter()
                .any(|record| record.tool_call_id.as_deref() == Some(tool_call_id))
        })
    }

    fn records(&self, scope: Scope) -> Box<dyn Iterator<Item = &ChangeRecord> + '_> {
        match scope {
            Scope::Session => Box::new(self.groups.iter().flat_map(|g| g.changes.iter())),
            Scope::Group(index) => Box::new(
                self.groups
                    .get(index)
                    .into_iter()
                    .flat_map(|g| g.changes.iter()),
            ),
        }
    }

    /// Net change per file in `scope`, in order of first change. Files whose changes
    /// cancel out are included (see [`FileSummary::is_unchanged`]).
    pub fn files(&self, scope: Scope) -> Vec<FileSummary> {
        let mut order: Vec<PathBuf> = Vec::new();
        let mut files: HashMap<PathBuf, FileSummary> = HashMap::new();
        for record in self.records(scope) {
            let change = &record.change;
            match files.get_mut(&change.path) {
                Some(summary) => {
                    summary.end = change.after.clone();
                    summary.edits += 1;
                    summary.stats = None;
                }
                None => {
                    order.push(change.path.clone());
                    files.insert(
                        change.path.clone(),
                        FileSummary {
                            path: change.path.clone(),
                            start: change.before.clone(),
                            end: change.after.clone(),
                            edits: 1,
                            stats: Some((change.added, change.removed)),
                        },
                    );
                }
            }
        }
        order
            .into_iter()
            .filter_map(|path| files.remove(&path))
            .collect()
    }

    /// Reverting the changes in `scope` (of one file, or all): each file goes back to
    /// its content before the scope.
    pub fn revert_targets(&self, scope: Scope, path: Option<&Path>) -> Vec<FileTarget> {
        self.files(scope)
            .into_iter()
            .filter(|file| path.is_none_or(|p| p == file.path))
            .filter(|file| !file.is_unchanged())
            .filter(|file| {
                !matches!(scope, Scope::Group(_))
                    || self.revert_status(scope, Some(&file.path)) != RevertStatus::Reverted
            })
            .map(|file| FileTarget {
                path: file.path,
                expected: file.end,
                target: file.start,
            })
            .collect()
    }

    /// Restoring the checkpoint before group `index`: every file changed in that group
    /// or later goes back to its content at that point.
    pub fn checkpoint_targets(&self, index: usize) -> Vec<FileTarget> {
        let later = ChangeLog {
            groups: self.groups.get(index..).unwrap_or_default().to_vec(),
        };
        let current = self.files(Scope::Session);
        later
            .files(Scope::Session)
            .into_iter()
            .filter_map(|file| {
                let end = current.iter().find(|c| c.path == file.path)?.end.clone();
                (!end.same_as(&file.start)).then_some(FileTarget {
                    path: file.path,
                    expected: end,
                    target: file.start,
                })
            })
            .collect()
    }

    /// Moving the files from the state at the end of branch `from` to the end of branch
    /// `to` (both logs of the same session).
    pub fn transition_targets(from: &ChangeLog, to: &ChangeLog) -> Vec<FileTarget> {
        let flat = |log: &ChangeLog| -> Vec<ChangeRecord> {
            log.groups.iter().flat_map(|g| g.changes.clone()).collect()
        };
        let (from, to) = (flat(from), flat(to));
        let common = from
            .iter()
            .zip(&to)
            .take_while(|(a, b)| a.entry_id == b.entry_id && a.change == b.change)
            .count();
        let (from, to) = (&from[common..], &to[common..]);

        let mut order: Vec<&Path> = Vec::new();
        for record in from.iter().chain(to) {
            if !order.contains(&record.change.path.as_path()) {
                order.push(&record.change.path);
            }
        }
        let first_before = |records: &[ChangeRecord], path: &Path| {
            records
                .iter()
                .find(|r| r.change.path == path)
                .map(|r| r.change.before.clone())
        };
        let last_after = |records: &[ChangeRecord], path: &Path| {
            records
                .iter()
                .rev()
                .find(|r| r.change.path == path)
                .map(|r| r.change.after.clone())
        };
        order
            .into_iter()
            .filter_map(|path| {
                let expected = last_after(from, path).or_else(|| first_before(to, path))?;
                let target = last_after(to, path).or_else(|| first_before(from, path))?;
                (!expected.same_as(&target)).then(|| FileTarget {
                    path: path.to_path_buf(),
                    expected,
                    target,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::BlobStore;
    use zp_ai::{ToolCall, ToolResultMessage};
    use zp_session::Session;

    fn tool_result(
        store: &BlobStore,
        id: &str,
        path: &str,
        before: Option<&str>,
        after: &str,
    ) -> AgentMessage {
        let call = ToolCall {
            id: id.into(),
            name: "edit".into(),
            arguments: Value::Null,
        };
        let mut result = ToolResultMessage::text(&call, "ok", false);
        result.details = Some(
            RecordedChange::store(store, path, before, Some(after))
                .unwrap()
                .to_details(),
        );
        AgentMessage::Llm(Message::ToolResult(result))
    }

    fn texts(
        store: &BlobStore,
        targets: &[FileTarget],
    ) -> Vec<(String, Option<String>, Option<String>)> {
        targets
            .iter()
            .map(|t| {
                let load = |c: &ContentRef| c.load(store).unwrap().map(|s| s.to_string());
                (
                    t.path.display().to_string(),
                    load(&t.expected),
                    load(&t.target),
                )
            })
            .collect()
    }

    #[test]
    fn restore_sources_track_partial_reverts_undo_and_branches() {
        let store = BlobStore::in_memory();
        let mut session = Session::in_memory(Path::new("/p"));
        session.append_message(AgentMessage::user("edit")).unwrap();
        session
            .append_message(tool_result(&store, "a1", "/p/a", Some("zero"), "one"))
            .unwrap();
        session
            .append_message(tool_result(&store, "a2", "/p/a", Some("one"), "two"))
            .unwrap();
        session
            .append_message(tool_result(&store, "b", "/p/b", None, "created"))
            .unwrap();
        let original_leaf = session.leaf_id().unwrap().to_owned();
        let original = ChangeLog::from_path(&session.branch_path(None));
        let restored = RecordedChange::store(&store, "/p/a", Some("two"), Some("zero")).unwrap();
        let message = restore_message_with_sources(
            "Revert a",
            &[restored],
            &original.revert_sources(Scope::Group(0), None),
            Path::new("/p"),
        );
        let details: RestoreDetails =
            serde_json::from_value(message.details.clone().unwrap()).unwrap();
        assert_eq!(
            details.sources.len(),
            2,
            "only successful paths are recorded"
        );
        session
            .append_message(AgentMessage::Custom(message))
            .unwrap();
        let log = ChangeLog::from_path(&session.branch_path(None));
        assert!(log.tool_reverted("a1"));
        assert!(log.tool_reverted("a2"));
        assert!(!log.tool_reverted("b"));
        assert_eq!(
            log.revert_status(Scope::Group(0), None),
            RevertStatus::Partial
        );
        assert_eq!(log.revert_targets(Scope::Group(0), None).len(), 1);

        let redone = RecordedChange::store(&store, "/p/a", Some("zero"), Some("two")).unwrap();
        session
            .append_message(AgentMessage::Custom(restore_message_with_sources(
                "Undo restore",
                &[redone],
                &log.revert_sources(Scope::Group(1), None),
                Path::new("/p"),
            )))
            .unwrap();
        let log = ChangeLog::from_path(&session.branch_path(None));
        assert!(!log.tool_reverted("a1"));
        assert!(!log.tool_reverted("a2"));
        assert_eq!(
            log.revert_status(Scope::Group(1), None),
            RevertStatus::Reverted
        );
        assert_eq!(
            log.revert_status(Scope::Group(0), None),
            RevertStatus::Active
        );
        session.branch(&original_leaf).unwrap();
        assert_eq!(
            ChangeLog::from_path(&session.branch_path(None)).revert_status(Scope::Group(0), None),
            RevertStatus::Active
        );
    }

    #[test]
    fn legacy_restore_records_match_merged_changes() {
        let store = BlobStore::in_memory();
        let mut session = Session::in_memory(Path::new("/p"));
        session.append_message(AgentMessage::user("first")).unwrap();
        session
            .append_message(tool_result(
                &store,
                "first",
                "/p/a",
                Some("one\ntwo\nthree\n"),
                "ONE\ntwo\nthree\n",
            ))
            .unwrap();
        session
            .append_message(AgentMessage::user("second"))
            .unwrap();
        session
            .append_message(tool_result(
                &store,
                "second",
                "/p/a",
                Some("ONE\ntwo\nthree\n"),
                "ONE\ntwo\nTHREE\n",
            ))
            .unwrap();
        let restored = RecordedChange::store(
            &store,
            "/p/a",
            Some("ONE\ntwo\nTHREE\n"),
            Some("one\ntwo\nTHREE\n"),
        )
        .unwrap();
        let mut message = restore_message("Reverted first", &[restored], Path::new("/p"));
        message
            .details
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("sources");
        session
            .append_message(AgentMessage::Custom(message))
            .unwrap();
        let mut log = ChangeLog::from_path(&session.branch_path(None));
        log.resolve_reverts(Some(&store));
        assert!(log.tool_reverted("first"));
        assert!(!log.tool_reverted("second"));
        assert_eq!(
            log.revert_status(Scope::Group(0), None),
            RevertStatus::Reverted
        );
        assert_eq!(
            log.revert_status(Scope::Group(1), None),
            RevertStatus::Active
        );
    }

    #[test]
    fn groups_summaries_and_targets() {
        let store = BlobStore::in_memory();
        let mut session = Session::in_memory(Path::new("/p"));
        session.append_message(AgentMessage::user("first")).unwrap();
        session
            .append_message(tool_result(&store, "c1", "/p/a", Some("a0"), "a1"))
            .unwrap();
        session
            .append_message(tool_result(&store, "c2", "/p/b", None, "b1"))
            .unwrap();
        session
            .append_message(AgentMessage::user("second"))
            .unwrap();
        session
            .append_message(tool_result(&store, "c3", "/p/a", Some("a1"), "a2"))
            .unwrap();
        let fork_point = session.leaf_id().unwrap().to_owned();
        session
            .append_message(AgentMessage::user("third, no changes"))
            .unwrap();

        let log = ChangeLog::from_path(&session.branch_path(None));
        assert_eq!(log.groups.len(), 3);
        assert_eq!(log.groups[1].label(), "Turn 2: second");
        assert_eq!(log.group_of_tool_call("c3"), Some(1));
        let indices: Vec<usize> = log.groups_with_changes().map(|(i, _)| i).collect();
        assert_eq!(indices, [1, 0]);

        let files = log.files(Scope::Session);
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].edits, files[0].stats), (2, None));
        assert_eq!(files[1].stats, Some((1, 0)));

        let s = |x: &str| Some(x.to_owned());
        assert_eq!(
            texts(&store, &log.revert_targets(Scope::Session, None)),
            [
                ("/p/a".into(), s("a2"), s("a0")),
                ("/p/b".into(), s("b1"), None)
            ]
        );
        assert_eq!(
            texts(
                &store,
                &log.revert_targets(Scope::Group(0), Some(Path::new("/p/a")))
            ),
            [("/p/a".into(), s("a1"), s("a0"))]
        );
        assert_eq!(
            texts(&store, &log.checkpoint_targets(1)),
            [("/p/a".into(), s("a2"), s("a1"))]
        );

        // A sibling branch from after turn 2: a goes another way, c is new.
        session.branch(&fork_point).unwrap();
        session.append_message(AgentMessage::user("other")).unwrap();
        session
            .append_message(tool_result(&store, "c4", "/p/a", Some("a2"), "a3"))
            .unwrap();
        session
            .append_message(tool_result(&store, "c5", "/p/c", None, "c1"))
            .unwrap();
        let other = ChangeLog::from_path(&session.branch_path(None));
        assert_eq!(
            texts(&store, &ChangeLog::transition_targets(&log, &other)),
            [
                ("/p/a".into(), s("a2"), s("a3")),
                ("/p/c".into(), None, s("c1"))
            ]
        );
        assert_eq!(
            texts(&store, &ChangeLog::transition_targets(&other, &log)),
            [
                ("/p/a".into(), s("a3"), s("a2")),
                ("/p/c".into(), s("c1"), None)
            ]
        );
        // Back before any change.
        let empty = ChangeLog::default();
        assert_eq!(
            texts(&store, &ChangeLog::transition_targets(&log, &empty)),
            [
                ("/p/a".into(), s("a2"), s("a0")),
                ("/p/b".into(), s("b1"), None)
            ]
        );

        // A restore is a group of its own and counts toward the session's net change.
        let restored = RecordedChange::store(&store, "/p/a", Some("a3"), Some("a0")).unwrap();
        let message = restore_message("Reverted a", &[restored], Path::new("/p"));
        assert!(
            message.content[0]
                .as_text()
                .unwrap()
                .contains("restored 1 file(s)")
        );
        assert_eq!(restore_summary(&message).as_deref(), Some("Reverted a — a"));
        session
            .append_message(AgentMessage::Custom(message))
            .unwrap();
        let log = ChangeLog::from_path(&session.branch_path(None));
        assert_eq!(log.groups.last().unwrap().label(), "Reverted a");
        let files = log.files(Scope::Session);
        assert!(
            files
                .iter()
                .find(|f| f.path == Path::new("/p/a"))
                .unwrap()
                .is_unchanged()
        );
        assert_eq!(
            log.revert_targets(Scope::Session, None).len(),
            2,
            "b and c remain"
        );
    }
}
