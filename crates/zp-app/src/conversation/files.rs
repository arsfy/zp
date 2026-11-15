//! File changes of a conversation: inline diffs of tool calls, reverts, checkpoints and
//! restoring files when moving to another branch.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Context, PromptLevel, Task, Window};
use zp_core::changes::{ChangeLog, GroupKind, Scope};
use zp_core::checkpoint::{BlobStore, RecordedChange};
use zp_core::diff::{DEFAULT_CONTEXT, FileDiff};
use zp_core::restore::{FileTarget, Outcome, RestorePlan};

use super::{ConversationEvent, ConversationView};
use crate::diff_view::{DiffItem, UnifiedRow, unified_rows};

/// The diff of one tool call, ready to render.
pub struct ToolDiff {
    pub diff: FileDiff,
    pub rows: Vec<UnifiedRow>,
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
        self.coding
            .as_ref()
            .map(|coding| coding.change_log())
            .unwrap_or_default()
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
    ) -> Option<DiffItem> {
        let (before, after) = load_pair(self.blobs().as_ref(), change).ok()?;
        Some(DiffItem {
            display: self.display_path(&change.path),
            path: change.path.clone(),
            caption: format!("Change by tool call {}", short_id(call_id)),
            before,
            after,
            revert: None,
        })
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
                Some(DiffItem {
                    display: self.display_path(&file.path),
                    path: file.path,
                    caption: caption.clone(),
                    before,
                    after,
                    revert: Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                        let path = path.clone();
                        let _ =
                            this.update(cx, |view, cx| view.revert(scope, Some(path), window, cx));
                    })),
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
        self.run_restore(targets, Some(description), window, cx);
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
        self.run_restore(targets, Some(description), window, cx);
    }

    fn run_restore(
        &mut self,
        targets: Vec<FileTarget>,
        record: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self.restore_files(targets, record, window, cx);
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                let _ = this.update(cx, |this, cx| this.report(Err(error), cx));
            }
        })
        .detach();
    }

    /// Restores files (asking before overwriting files changed outside ZP). With
    /// `record`, the restore is added to the transcript so the model knows. Resolves to
    /// whether it went ahead.
    pub(super) fn restore_files(
        &mut self,
        targets: Vec<FileTarget>,
        record: Option<String>,
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
            let changes = plan.apply(overwrite, &blobs)?;
            this.update(cx, |this, cx| {
                if let Some(description) = record {
                    coding.record_restore(&description, &changes)?;
                    this.reload_transcript();
                }
                cx.emit(ConversationEvent::FilesChanged);
                cx.notify();
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
