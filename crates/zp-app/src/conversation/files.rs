//! File changes of a conversation: inline diffs of tool calls, reverts, checkpoints and
//! restoring files when moving to another branch.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, PromptLevel, Task, Window};
use zp_core::changes::{ChangeLog, GroupKind, RestoreSource, RevertStatus, Scope};
use zp_core::checkpoint::{BlobStore, RecordedChange};
use zp_core::diff::{DEFAULT_CONTEXT, FileDiff};
use zp_core::restore::{FileTarget, Outcome, RestorePlan};

use super::transcript::{Row, ToolStatus};
use super::{ConversationEvent, ConversationView};
use crate::diff_view::{DiffItem, UnifiedRow, unified_rows};

/// The diff of one tool call, ready to render.
pub struct ToolDiff {
    pub diff: FileDiff,
    pub rows: Vec<UnifiedRow>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TurnChanges {
    pub group: usize,
    pub files: Vec<TurnChangeFile>,
    pub added: usize,
    pub removed: usize,
    pub status: RevertStatus,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TurnChangeFile {
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
}

pub(super) struct RestoreRecord {
    description: String,
    sources: Vec<RestoreSource>,
}

/// What a branch change does after the files are dealt with.
pub(super) type BranchAction =
    Box<dyn FnOnce(&mut ConversationView, &mut Window, &mut Context<ConversationView>)>;

impl ConversationView {
    /// Where this conversation's file versions are kept.
    pub fn blobs(&self) -> Option<BlobStore> {
        self.coding.as_ref().map(|coding| coding.blobs().clone())
    }

    /// The file changes along the shown branch.
    pub fn change_log(&self) -> ChangeLog {
        if let Some(log) = self.change_log_cache.borrow().as_ref() {
            return log.clone();
        }
        let log = self
            .coding
            .as_ref()
            .map(|coding| coding.change_log())
            .unwrap_or_default();
        self.change_log_cache.borrow_mut().replace(log.clone());
        log
    }

    /// Net changes for the answer ending at `row`, cached to avoid reading blobs on repaint.
    pub(crate) fn turn_changes_for_row(&self, row: usize) -> Option<Rc<TurnChanges>> {
        if !matches!(self.transcript.rows().get(row), Some(Row::Assistant { .. }))
            || matches!(
                self.transcript.rows().get(row + 1),
                Some(Row::Assistant { .. })
            )
            || (self.is_busy() && row + 1 == self.transcript.len())
        {
            return None;
        }
        if let Some(changes) = self.turn_changes.borrow().get(&row) {
            return changes.clone();
        }
        let changes = self.compute_turn_changes(row).map(Rc::new);
        self.turn_changes.borrow_mut().insert(row, changes.clone());
        changes
    }

    fn compute_turn_changes(&self, row: usize) -> Option<TurnChanges> {
        let user_row = self.transcript.rows()[..=row]
            .iter()
            .rposition(|row| matches!(row, Row::User { .. }))?;
        let entry = self.entry_for_row(user_row)?;
        let log = self.change_log();
        let group = log
            .groups
            .iter()
            .position(|group| group.entry_id.as_deref() == Some(&entry))?;
        let files = log.files(Scope::Group(group));
        let blobs = self.blobs()?;
        let mut changes = TurnChanges {
            group,
            files: Vec::new(),
            added: 0,
            removed: 0,
            status: log.revert_status(Scope::Group(group), None),
        };
        for file in files.into_iter().filter(|file| !file.is_unchanged()) {
            let (added, removed) = match file.stats {
                Some(stats) => stats,
                None => {
                    let before = file.start.load(&blobs).ok()?;
                    let after = file.end.load(&blobs).ok()?;
                    zp_core::diff::line_stats(before.as_deref(), after.as_deref())
                }
            };
            changes.files.push(TurnChangeFile {
                path: file.path,
                added,
                removed,
            });
            changes.added += added;
            changes.removed += removed;
        }
        (!changes.files.is_empty()).then_some(changes)
    }

    /// Opens the whole turn's diff with the selected file shown first.
    pub(super) fn open_turn_diff(&self, scope: Scope, path: Option<&Path>, cx: &mut Context<Self>) {
        let items = self.diff_items(scope, cx);
        let index = match path {
            Some(path) => items.iter().position(|item| item.path == path),
            None => (!items.is_empty()).then_some(0),
        };
        if let Some(index) = index {
            cx.emit(ConversationEvent::OpenDiff { items, index });
        }
    }

    /// `path` relative to the project, for display.
    pub fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// The diff of the change made by tool call `call_id` (cached).
    pub(crate) fn tool_diff(
        &self,
        call_id: &str,
        change: &RecordedChange,
    ) -> Rc<Result<ToolDiff, String>> {
        if let Some(diff) = self.tool_diffs.borrow().get(call_id) {
            return diff.clone();
        }
        let result = match (self.blobs(), load_pair(self.blobs().as_ref(), change)) {
            (Some(_), Ok((before, after))) => {
                let diff = FileDiff::compute(before.as_deref(), after.as_deref(), DEFAULT_CONTEXT);
                let rows = unified_rows(&diff);
                Ok(ToolDiff { diff, rows })
            }
            (None, _) => Err("no session".to_owned()),
            (_, Err(error)) => Err(format!("{error:#}")),
        };
        let result = Rc::new(result);
        self.tool_diffs
            .borrow_mut()
            .insert(call_id.to_owned(), result.clone());
        result
    }

    /// The full diff of tool call `call_id`'s change.
    pub(crate) fn tool_diff_item(
        &self,
        call_id: &str,
        change: &RecordedChange,
        cx: &Context<Self>,
    ) -> Option<DiffItem> {
        let (before, after) = load_pair(self.blobs().as_ref(), change).ok()?;
        let this = cx.entity().downgrade();
        let id = call_id.to_owned();
        let reverted = self.change_log().tool_reverted(call_id);
        Some(DiffItem {
            display: self.display_path(&change.path),
            path: change.path.clone(),
            caption: format!("Change by tool call {}", short_id(call_id)),
            before,
            after,
            status: if reverted {
                RevertStatus::Reverted
            } else {
                RevertStatus::Active
            },
            revert: (!self.is_busy() && !reverted && !change.before.same_as(&change.after)).then(
                || {
                    Rc::new(move |window: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |view, cx| view.revert_tool_call(&id, window, cx));
                    }) as crate::diff_view::RevertFn
                },
            ),
        })
    }

    /// Reverts one tool call, preserving subsequent non-conflicting edits to the file.
    pub(crate) fn revert_tool_call(
        &mut self,
        call_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let log = self.change_log();
        if log.tool_reverted(call_id) {
            return;
        }
        let Some(change) = self
            .transcript
            .tool(call_id)
            .filter(|state| state.status == ToolStatus::Done)
            .and_then(|state| state.details.as_ref())
            .and_then(RecordedChange::from_details)
        else {
            return;
        };
        let description = format!("Reverted edit to {}", self.display_path(&change.path));
        let target = FileTarget {
            path: change.path,
            expected: change.after,
            target: change.before,
        };
        let sources = log
            .groups
            .iter()
            .flat_map(|group| &group.changes)
            .filter(|record| record.tool_call_id.as_deref() == Some(call_id))
            .map(|record| RestoreSource {
                entry_id: record.entry_id.clone(),
                path: record.change.path.clone(),
            })
            .collect();
        self.run_restore(
            vec![target],
            RestoreRecord {
                description,
                sources,
            },
            window,
            cx,
        );
    }

    /// Diffs of the files changed in `scope`, each offering a revert.
    pub fn diff_items(&self, scope: Scope, cx: &Context<Self>) -> Vec<DiffItem> {
        let Some(blobs) = self.blobs() else {
            return Vec::new();
        };
        let log = self.change_log();
        let caption = match scope {
            Scope::Session => "Changes in this conversation".to_owned(),
            Scope::Group(index) => log
                .groups
                .get(index)
                .map(|group| group.label())
                .unwrap_or_default(),
        };
        let this = cx.entity().downgrade();
        log.files(scope)
            .into_iter()
            .filter(|file| !file.is_unchanged())
            .filter_map(|file| {
                let before = file.start.load(&blobs).ok()?;
                let after = file.end.load(&blobs).ok()?;
                let this = this.clone();
                let path = file.path.clone();
                let status = log.revert_status(scope, Some(&path));
                Some(DiffItem {
                    display: self.display_path(&file.path),
                    path: file.path,
                    caption: caption.clone(),
                    before,
                    after,
                    status,
                    revert: (status != RevertStatus::Reverted).then(|| {
                        Rc::new(move |window: &mut Window, cx: &mut App| {
                            let path = path.clone();
                            let _ = this
                                .update(cx, |view, cx| view.revert(scope, Some(path), window, cx));
                        }) as crate::diff_view::RevertFn
                    }),
                })
            })
            .collect()
    }

    /// Reverts the changes in `scope` (of one file, or all of them) and records it.
    pub(crate) fn revert(
        &mut self,
        scope: Scope,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let log = self.change_log();
        let targets = log.revert_targets(scope, path.as_deref());
        let what = match &path {
            Some(path) => self.display_path(path),
            None => "all changes".to_owned(),
        };
        let description = match scope {
            Scope::Session => format!("Reverted {what}"),
            Scope::Group(index) => match log.groups.get(index) {
                Some(group) if matches!(group.kind, GroupKind::Restore { .. }) => {
                    format!("Undid a restore of {what}")
                }
                Some(group) if path.is_some() => format!("Reverted {what} in turn {}", group.turn),
                Some(group) => format!("Reverted turn {}", group.turn),
                None => return,
            },
        };
        let sources = log.revert_sources(scope, path.as_deref());
        self.run_restore(
            targets,
            RestoreRecord {
                description,
                sources,
            },
            window,
            cx,
        );
    }

    /// Restores every file changed in group `index` or later to its state before it.
    pub(crate) fn restore_checkpoint(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let log = self.change_log();
        let Some(group) = log.groups.get(index) else {
            return;
        };
        let description = format!("Restored the checkpoint before turn {}", group.turn.max(1));
        let targets = log.checkpoint_targets(index);
        let sources = (index..log.groups.len())
            .flat_map(|index| log.revert_sources(Scope::Group(index), None))
            .collect();
        self.run_restore(
            targets,
            RestoreRecord {
                description,
                sources,
            },
            window,
            cx,
        );
    }

    fn run_restore(
        &mut self,
        targets: Vec<FileTarget>,
        record: RestoreRecord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if targets.is_empty() || self.is_busy() || self.coding.is_none() {
            return;
        }
        let files = targets
            .iter()
            .map(|target| {
                let path = self.display_path(&target.path);
                if target.target.is_absent() {
                    format!("{path} (delete)")
                } else {
                    path
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let answer = window.prompt(
            PromptLevel::Warning,
            "Revert these changes?",
            Some(&format!(
                "Restore the saved versions of {} file(s):\n\n{files}",
                targets.len()
            )),
            &["Revert", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(task) = this.update_in(cx, |this, window, cx| {
                this.restore_files(targets, Some(record), window, cx)
            }) else {
                return;
            };
            if let Err(error) = task.await {
                let _ = this.update(cx, |this, cx| this.report(Err(error), cx));
            }
        })
        .detach();
    }

    /// Restores files (asking before overwriting files changed outside ZP). With
    /// `record`, the restore is added to the transcript so the model knows. Resolves to
    /// whether it went ahead; it fails, after recording what was restored, when a file
    /// could not be restored or changed while the user was being asked.
    pub(super) fn restore_files(
        &mut self,
        targets: Vec<FileTarget>,
        record: Option<RestoreRecord>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<bool>> {
        let Some(coding) = self.coding.clone() else {
            return Task::ready(Ok(false));
        };
        if self.running {
            return Task::ready(Err(anyhow::anyhow!(
                "Stop the agent before restoring files."
            )));
        }
        let blobs = coding.blobs().clone();
        let plan = match RestorePlan::prepare(&targets, &blobs) {
            Ok(plan) => plan,
            Err(error) => return Task::ready(Err(error.context("Could not restore files"))),
        };
        let conflicts: Vec<String> = plan
            .conflicts()
            .map(|file| match file.outcome {
                Outcome::Conflict(kind) => {
                    format!("{} ({})", self.display_path(&file.path), kind.describe())
                }
                _ => self.display_path(&file.path),
            })
            .collect();
        let answer = (!conflicts.is_empty()).then(|| {
            window.prompt(
                PromptLevel::Warning,
                "Some files were changed outside ZP",
                Some(&format!(
                    "{}\n\nOverwrite them with the restored version, or leave them as they are?",
                    conflicts.join("\n")
                )),
                &["Overwrite", "Skip These Files", "Cancel"],
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let overwrite = match answer {
                None => false,
                Some(answer) => match answer.await {
                    Ok(0) => true,
                    Ok(1) => false,
                    _ => return Ok(false),
                },
            };
            let applied = plan.apply(overwrite, &blobs);
            this.update(cx, |this, cx| {
                // Record what was restored even when the rest failed or was skipped.
                if let Some(record) = record {
                    coding.record_restore_with_sources(
                        &record.description,
                        &applied.changes,
                        &record.sources,
                    )?;
                    this.reload_transcript();
                    if !applied.changes.is_empty() {
                        cx.emit(ConversationEvent::FilesRestored);
                    }
                }
                cx.emit(ConversationEvent::FilesChanged);
                cx.notify();
                if let Some(error) = applied.error {
                    return Err(error.context("Could not restore every file"));
                }
                if !applied.changed_since_prepare.is_empty() {
                    let names: Vec<String> = applied
                        .changed_since_prepare
                        .iter()
                        .map(|path| this.display_path(path))
                        .collect();
                    anyhow::bail!(
                        "These files changed while ZP was asking and were left as they are: {}",
                        names.join(", ")
                    );
                }
                anyhow::Ok(true)
            })?
        })
    }

    /// Moves to the branch ending at `target_leaf` with `action`, first offering to
    /// restore the files the agent changed on the way.
    pub(super) fn change_branch(
        &mut self,
        target_leaf: Option<String>,
        action: BranchAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        if self.running {
            return;
        }
        let targets = ChangeLog::transition_targets(
            &coding.change_log(),
            &coding.change_log_at(target_leaf.as_deref()),
        );
        if targets.is_empty() {
            action(self, window, cx);
            return;
        }
        let names: Vec<String> = targets
            .iter()
            .map(|target| self.display_path(&target.path))
            .collect();
        let answer = window.prompt(
            PromptLevel::Info,
            "Restore files to match?",
            Some(&format!(
                "The agent changed {} file(s) after that point: {}.\n\nRestore them to how they were there, or keep the current files?",
                names.len(),
                names.join(", ")
            )),
            &["Restore Files", "Keep Files", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let restore = match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return,
            };
            if restore {
                let Ok(task) = this.update_in(cx, |this, window, cx| {
                    this.restore_files(targets, None, window, cx)
                }) else {
                    return;
                };
                match task.await {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(error) => {
                        let _ = this.update(cx, |this, cx| this.report(Err(error), cx));
                        return;
                    }
                }
            }
            let _ = this.update_in(cx, |this, window, cx| action(this, window, cx));
        })
        .detach();
    }
}

/// Loads both versions of a change.
#[allow(clippy::type_complexity)]
fn load_pair(
    blobs: Option<&BlobStore>,
    change: &RecordedChange,
) -> anyhow::Result<(Option<Arc<str>>, Option<Arc<str>>)> {
    let blobs = blobs.ok_or_else(|| anyhow::anyhow!("no session"))?;
    Ok((change.before.load(blobs)?, change.after.load(blobs)?))
}

fn short_id(id: &str) -> &str {
    let end = id.char_indices().nth(12).map_or(id.len(), |(i, _)| i);
    &id[..end]
}
