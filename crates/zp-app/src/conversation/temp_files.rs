//! Temporary files a run created: tracked while it runs, then shown as a one-line card
//! above the turn's changes, where their size can be measured and they can be deleted.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AnyElement, Context, ElementId, FontWeight, IntoElement, div, prelude::*, px};
use serde_json::Value;
use zp_agent::AgentEvent;
use zp_core::temp::{Snapshot, scratch_roots};

use super::ConversationView;
use crate::theme::ActiveTheme;

/// Names listed in the card before "+N more".
const NAMES_SHOWN: usize = 3;

/// What a running run has named in the temporary directories so far.
pub(super) struct TempRun {
    snapshot: Arc<Snapshot>,
    mentioned: BTreeSet<PathBuf>,
}

/// Temporary files a finished run created, shown under its last row.
pub(super) struct TempUse {
    snapshot: Arc<Snapshot>,
    paths: Vec<PathBuf>,
    size: Option<Measure>,
    state: Cleanup,
}

#[derive(Clone, Copy)]
enum Measure {
    Measuring,
    Bytes(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cleanup {
    Present,
    Removing,
    Removed,
    Failed(String),
}

/// Every string in `value` (the arguments of a call), so paths in any field are found.
/// Borrowed: a write's arguments hold the whole file.
fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(fields) => fields.values().for_each(|field| strings(field, out)),
        _ => {}
    }
}

impl ConversationView {
    /// Lists the temporary directories as a run starts.
    pub(super) fn start_temp_tracking(&mut self) {
        let snapshot = Snapshot::take(scratch_roots(&self.root));
        self.temp_run = Some(TempRun {
            snapshot: Arc::new(snapshot),
            mentioned: BTreeSet::new(),
        });
    }

    /// Notes the temporary paths a tool call names, in its arguments or its result.
    pub(super) fn track_temp_paths(&mut self, event: &AgentEvent) {
        let Some(run) = &mut self.temp_run else {
            return;
        };
        let mut texts = Vec::new();
        match event {
            AgentEvent::ToolExecutionStart { args, .. } => strings(args, &mut texts),
            AgentEvent::ToolExecutionEnd { result, .. } => texts.extend(
                result
                    .content
                    .iter()
                    .filter_map(zp_ai::UserContent::as_text),
            ),
            _ => return,
        }
        for text in texts {
            let found = run.snapshot.mentioned(text);
            run.mentioned.extend(found);
        }
    }

    /// As a run finishes: what it created in the temporary directories, for the card
    /// under its last row.
    pub(super) fn finish_temp_tracking(&mut self) {
        let Some(run) = self.temp_run.take() else {
            return;
        };
        let paths = run.snapshot.created(&run.mentioned);
        let Some(row) = self.transcript.len().checked_sub(1) else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        // A later run of the same turn adds to what is listed.
        match self.temp_uses.get_mut(&row) {
            Some(existing) if existing.state == Cleanup::Present => {
                for path in paths {
                    if !existing.paths.contains(&path) {
                        existing.paths.push(path);
                    }
                }
                existing.size = None;
            }
            _ => {
                self.temp_uses.insert(
                    row,
                    TempUse {
                        snapshot: run.snapshot,
                        paths,
                        size: None,
                        state: Cleanup::Present,
                    },
                );
            }
        }
        self.list_state.remeasure_items(row..row + 1);
    }

    /// Temporary paths listed under `row` (tests).
    #[cfg(test)]
    pub(crate) fn temp_paths(&self, row: usize) -> Option<Vec<PathBuf>> {
        self.temp_uses.get(&row).map(|used| used.paths.clone())
    }

    /// The measured size of the temporary files under `row` (tests).
    #[cfg(test)]
    pub(crate) fn temp_size(&self, row: usize) -> Option<u64> {
        match self.temp_uses.get(&row)?.size {
            Some(Measure::Bytes(bytes)) => Some(bytes),
            _ => None,
        }
    }

    /// Measures the temporary files under `row` in the background.
    pub(crate) fn measure_temp(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(used) = self.temp_uses.get_mut(&row) else {
            return;
        };
        used.size = Some(Measure::Measuring);
        let paths = used.paths.clone();
        let task = cx.background_executor().spawn(async move {
            paths
                .iter()
                .map(|path| zp_core::temp::size(path))
                .sum::<u64>()
        });
        cx.spawn(async move |this, cx| {
            let bytes = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(used) = this.temp_uses.get_mut(&row) {
                    used.size = Some(Measure::Bytes(bytes));
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Deletes the temporary files under `row` (not while the agent works: it may be
    /// using them).
    pub(crate) fn clean_temp(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(used) = self.temp_uses.get_mut(&row) else {
            return;
        };
        if !matches!(used.state, Cleanup::Present | Cleanup::Failed(_)) {
            return;
        }
        used.state = Cleanup::Removing;
        let (snapshot, paths) = (used.snapshot.clone(), used.paths.clone());
        let task = cx
            .background_executor()
            .spawn(async move { zp_core::temp::remove(&snapshot, &paths) });
        cx.spawn(async move |this, cx| {
            let failed = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(used) = this.temp_uses.get_mut(&row) {
                    used.state = match failed.first() {
                        None => Cleanup::Removed,
                        Some((path, error)) => {
                            Cleanup::Failed(format!("Could not delete {}: {error}", path.display()))
                        }
                    };
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The one-line card of the temporary files `row`'s run created.
    pub(super) fn render_temp_use(&self, row: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let used = self.temp_uses.get(&row)?;
        let colors = cx.theme().colors.clone();
        let busy = self.is_busy();
        let names: Vec<String> = used
            .paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let mut listed = names
            .iter()
            .take(NAMES_SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if names.len() > NAMES_SHOWN {
            listed.push_str(&format!(" +{} more", names.len() - NAMES_SHOWN));
        }
        let full = used
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (title, detail) = match &used.state {
            Cleanup::Removed => ("Temporary files deleted".to_owned(), listed.clone()),
            _ if used.paths.len() == 1 => ("Temporary file".to_owned(), listed.clone()),
            _ => (
                format!("{} temporary files", used.paths.len()),
                listed.clone(),
            ),
        };
        let button = |id: &'static str, label: String, enabled: bool| {
            div()
                .id(ElementId::NamedInteger(id.into(), row as u64))
                .debug_selector(move || id.into())
                .flex_shrink_0()
                .px_2()
                .py_0p5()
                .rounded(px(4.))
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .child(label)
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover_style(|style| style.bg(colors.surface_hover).text_color(colors.text))
                })
                .when(!enabled, |button| button.opacity(0.4))
        };
        let size = match used.size {
            Some(Measure::Bytes(bytes)) => Some(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(zp_tools::format_size(
                        usize::try_from(bytes).unwrap_or(usize::MAX),
                    ))
                    .into_any_element(),
            ),
            Some(Measure::Measuring) => Some(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child("Measuring…")
                    .into_any_element(),
            ),
            None => None,
        };
        let present = matches!(used.state, Cleanup::Present | Cleanup::Failed(_));
        Some(
            div()
                .id(ElementId::NamedInteger("temp-files".into(), row as u64))
                .debug_selector(|| "temp-files".into())
                .mt_2()
                .flex()
                .flex_col()
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .child(
                    div()
                        .id(ElementId::NamedInteger(
                            "temp-files-line".into(),
                            row as u64,
                        ))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .text_size(px(12.))
                        .tooltip(crate::ui::tooltip(full))
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(cx.theme().metrics.mono_font.clone())
                                .text_size(px(11.))
                                .text_color(colors.text_muted)
                                .child(detail),
                        )
                        .when(present, |line| {
                            line.children(size)
                                .when(used.size.is_none(), |line| {
                                    line.child(
                                        button("temp-files-size", "Size".to_owned(), true)
                                            .tooltip(crate::ui::tooltip("Measure their size"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.measure_temp(row, cx)
                                            })),
                                    )
                                })
                                .child(
                                    button("temp-files-clean", "Clean up".to_owned(), !busy)
                                        .tooltip(crate::ui::tooltip(if busy {
                                            "Available when the agent is done"
                                        } else {
                                            "Delete these temporary files"
                                        }))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.clean_temp(row, cx)
                                        })),
                                )
                        })
                        .when(used.state == Cleanup::Removing, |line| {
                            line.child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(colors.text_muted)
                                    .child("Deleting…"),
                            )
                        }),
                )
                .when_some(
                    match &used.state {
                        Cleanup::Failed(error) => Some(error.clone()),
                        _ => None,
                    },
                    |card, error| {
                        card.child(
                            div()
                                .px_3()
                                .pb_1p5()
                                .text_size(px(12.))
                                .text_color(colors.error)
                                .whitespace_normal()
                                .child(error),
                        )
                    },
                )
                .into_any_element(),
        )
    }
}

/// Temporary file cards by the row they are shown under.
pub(super) type TempUses = HashMap<usize, TempUse>;
