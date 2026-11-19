//! The `Changes` panel: files changed in the active conversation (overall or per turn)
//! with revert actions, and the git working tree.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FontWeight, IntoElement,
    Render, Rgba, SharedString, StyleRefinement, Subscription, Window, div, prelude::*, px,
};
use zp_core::changes::{ChangeLog, FileSummary, GroupKind, RevertStatus, Scope};
use zp_core::checkpoint::BlobStore;

use crate::conversation::{ConversationEvent, ConversationView};
use crate::diff_view::DiffItem;
use crate::git_panel::{GitPanel, GitPanelEvent};
use crate::theme::ActiveTheme;
use crate::ui::tooltip;

/// What the panel lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangesMode {
    /// Net changes of the whole conversation (current branch).
    Session,
    /// Changes per turn, with checkpoints.
    Turns,
    /// The git repository: staging, commits, branches and history.
    Git,
}

/// How a file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Added,
    Modified,
    Deleted,
}

/// A changed file as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    pub path: PathBuf,
    pub display: String,
    pub kind: FileKind,
    pub added: usize,
    pub removed: usize,
    pub status: RevertStatus,
}

/// A turn (or restore) with its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRow {
    /// Index into the change log's groups.
    pub index: usize,
    pub label: String,
    pub is_restore: bool,
    pub status: RevertStatus,
    pub files: Vec<FileRow>,
}

/// A row action of the panel.
type PanelAction = Box<dyn Fn(&mut ChangesPanel, &mut Window, &mut Context<ChangesPanel>)>;

pub enum ChangesPanelEvent {
    /// Show these diffs, starting at `index`.
    OpenDiff { items: Vec<DiffItem>, index: usize },
    /// The branch or commits changed.
    RepositoryChanged,
}

pub struct ChangesPanel {
    root: PathBuf,
    mode: ChangesMode,
    conversation: Option<Entity<ConversationView>>,
    files: Vec<FileRow>,
    groups: Vec<GroupRow>,
    git: Entity<GitPanel>,
    _events: Option<Subscription>,
    _git_events: Subscription,
}

impl EventEmitter<ChangesPanelEvent> for ChangesPanel {}

impl ChangesPanel {
    pub fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let git = cx.new(|cx| GitPanel::new(root.clone(), cx));
        let git_events = cx.subscribe(&git, |_, _, event: &GitPanelEvent, cx| match event {
            GitPanelEvent::OpenDiff { items, index } => cx.emit(ChangesPanelEvent::OpenDiff {
                items: items.clone(),
                index: *index,
            }),
            GitPanelEvent::RepositoryChanged => cx.emit(ChangesPanelEvent::RepositoryChanged),
        });
        Self {
            root,
            mode: ChangesMode::Git,
            conversation: None,
            files: Vec::new(),
            groups: Vec::new(),
            git,
            _events: None,
            _git_events: git_events,
        }
    }

    /// The Git view (tests).
    #[cfg(any(test, feature = "visual-tests"))]
    pub fn git(&self) -> &Entity<GitPanel> {
        &self.git
    }

    /// Follows `conversation` (the active tab).
    pub fn set_conversation(
        &mut self,
        conversation: Option<Entity<ConversationView>>,
        cx: &mut Context<Self>,
    ) {
        if self.conversation == conversation {
            return;
        }
        self._events = conversation.as_ref().map(|conversation| {
            cx.subscribe(conversation, |this, _, event, cx| match event {
                ConversationEvent::FilesChanged | ConversationEvent::RunFinished => {
                    this.refresh(cx);
                    if this.mode == ChangesMode::Git {
                        this.refresh_git(cx);
                    }
                }
                ConversationEvent::StatusChanged => cx.notify(),
                _ => {}
            })
        });
        self.conversation = conversation;
        self.refresh(cx);
    }

    pub fn mode(&self) -> ChangesMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ChangesMode, cx: &mut Context<Self>) {
        self.mode = mode;
        if mode == ChangesMode::Git {
            self.refresh_git(cx);
        }
        cx.notify();
    }

    /// Number of files the conversation changed (net).
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Rows of the session view.
    #[cfg(test)]
    pub fn files(&self) -> &[FileRow] {
        &self.files
    }

    /// Groups of the turns view, newest first.
    #[cfg(test)]
    pub fn groups(&self) -> &[GroupRow] {
        &self.groups
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn rows(&self, log: &ChangeLog, scope: Scope, blobs: Option<&BlobStore>) -> Vec<FileRow> {
        log.files(scope)
            .into_iter()
            .filter(|file| !file.is_unchanged())
            .map(|file| {
                let mut row = self.file_row(&file, blobs);
                row.status = log.revert_status(scope, Some(&file.path));
                row
            })
            .collect()
    }

    fn file_row(&self, file: &FileSummary, blobs: Option<&BlobStore>) -> FileRow {
        let kind = if file.start.is_absent() {
            FileKind::Added
        } else if file.end.is_absent() {
            FileKind::Deleted
        } else {
            FileKind::Modified
        };
        let (added, removed) = file.stats.unwrap_or_else(|| {
            let load = |content: &zp_core::checkpoint::ContentRef| {
                blobs.and_then(|blobs| content.load(blobs).ok()).flatten()
            };
            zp_core::diff::line_stats(load(&file.start).as_deref(), load(&file.end).as_deref())
        });
        FileRow {
            display: self.display(&file.path),
            path: file.path.clone(),
            kind,
            added,
            removed,
            status: RevertStatus::Active,
        }
    }

    /// Recomputes the lists from the conversation's change log.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let (log, blobs) = match &self.conversation {
            Some(conversation) => {
                let view = conversation.read(cx);
                (view.change_log(), view.blobs())
            }
            None => (ChangeLog::default(), None),
        };
        self.files = self.rows(&log, Scope::Session, blobs.as_ref());
        self.groups = log
            .groups_with_changes()
            .map(|(index, group)| GroupRow {
                index,
                label: group.label(),
                is_restore: matches!(group.kind, GroupKind::Restore { .. }),
                status: log.revert_status(Scope::Group(index), None),
                files: self.rows(&log, Scope::Group(index), blobs.as_ref()),
            })
            .filter(|group| !group.files.is_empty())
            .collect();
        cx.notify();
    }

    /// Reloads the Git view.
    pub fn refresh_git(&mut self, cx: &mut Context<Self>) {
        self.git.update(cx, |git, cx| git.refresh(cx));
    }

    fn running(&self, cx: &App) -> bool {
        self.conversation
            .as_ref()
            .is_some_and(|c| c.read(cx).is_running())
    }

    /// Opens the diffs of `scope` at `path`.
    pub fn open_file(&mut self, scope: Scope, path: &Path, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversation.clone() else {
            return;
        };
        let items = conversation.update(cx, |view, cx| view.diff_items(scope, cx));
        let index = items.iter().position(|item| item.path == path).unwrap_or(0);
        if !items.is_empty() {
            cx.emit(ChangesPanelEvent::OpenDiff { items, index });
        }
    }

    pub fn revert(
        &mut self,
        scope: Scope,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(conversation) = self.conversation.clone() {
            conversation.update(cx, |view, cx| view.revert(scope, path, window, cx));
        }
    }

    pub fn restore_checkpoint(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(conversation) = self.conversation.clone() {
            conversation.update(cx, |view, cx| view.restore_checkpoint(index, window, cx));
        }
    }

    // ----- rendering -------------------------------------------------------------------

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let tab = |mode: ChangesMode, label: &'static str| {
            let active = self.mode == mode;
            div()
                .id(label)
                .px_2()
                .py_0p5()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(11.))
                .when(active, |this| {
                    this.bg(colors.surface_hover).text_color(colors.text)
                })
                .when(!active, |this| this.text_color(colors.text_muted))
                .hover_style(|s| s.text_color(colors.text))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .pt_3()
            .pb_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(colors.text_muted)
                    .child("CHANGES"),
            )
            .child(
                div()
                    .flex()
                    .gap_0p5()
                    .child(tab(ChangesMode::Session, "Session"))
                    .child(tab(ChangesMode::Turns, "Turns"))
                    .child(tab(ChangesMode::Git, "Git")),
            )
    }

    fn render_file(
        &self,
        id: SharedString,
        row: &FileRow,
        on_open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        on_revert: Option<PanelAction>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (letter, color) = kind_badge(row.kind, cx);
        let (name, dir) = split_path(&row.display);
        let group: SharedString = format!("{id}-group").into();
        div()
            .id(id)
            .group(group.clone())
            .flex()
            .items_center()
            .gap_1p5()
            .px_3()
            .py_0p5()
            .cursor_pointer()
            .hover_style(|s| s.bg(colors.surface_hover))
            .on_click(cx.listener(move |this, _, window, cx| on_open(this, window, cx)))
            .child(
                div()
                    .w(px(12.))
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(color)
                    .child(letter),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(div().flex_shrink_0().max_w_full().truncate().child(name))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(colors.text_muted)
                            .child(dir),
                    ),
            )
            .when_some(row.status.label(), |this, label| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child(label),
                )
            })
            .when_some(
                on_revert.filter(|_| row.status != RevertStatus::Reverted),
                |this, on_revert| {
                    this.child(
                        div()
                            .id("revert")
                            .invisible()
                            .group_hover_style(group, |s| s.visible())
                            .flex_shrink_0()
                            .px_1()
                            .rounded(px(3.))
                            .text_size(px(11.))
                            .text_color(colors.text_muted)
                            .hover_style(|s| s.text_color(colors.error).bg(colors.background))
                            .child("Revert")
                            .tooltip(tooltip("Undo these changes to the file"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                on_revert(this, window, cx)
                            })),
                    )
                },
            )
            .child(stats(row.added, row.removed, cx))
            .into_any_element()
    }

    fn render_session(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        if self.files.is_empty() {
            return empty("No files changed in this conversation yet.", cx);
        }
        let running = self.running(cx);
        let (added, removed) = self
            .files
            .iter()
            .fold((0, 0), |(a, r), row| (a + row.added, r + row.removed));
        let mut list = div().flex().flex_col().child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .child(
                    div()
                        .flex_1()
                        .child(format!("{} file(s) changed", self.files.len())),
                )
                .child(stats(added, removed, cx))
                .when(!running, |this| {
                    this.child(
                        small_button("revert-all", "Revert All", colors.error, cx)
                            .tooltip(tooltip("Undo every change of this conversation"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.revert(Scope::Session, None, window, cx)
                            })),
                    )
                }),
        );
        for (index, row) in self.files.iter().enumerate() {
            let open_path = row.path.clone();
            let revert_path = row.path.clone();
            let on_revert: Option<PanelAction> = (!running).then(|| {
                Box::new(
                    move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                        this.revert(Scope::Session, Some(revert_path.clone()), window, cx)
                    },
                ) as Box<_>
            });
            list = list.child(self.render_file(
                format!("session-file-{index}").into(),
                row,
                move |this, _, cx| this.open_file(Scope::Session, &open_path, cx),
                on_revert,
                cx,
            ));
        }
        list.into_any_element()
    }

    fn render_turns(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        if self.groups.is_empty() {
            return empty("No files changed in this conversation yet.", cx);
        }
        let running = self.running(cx);
        let mut list = div().flex().flex_col();
        for group in &self.groups {
            let index = group.index;
            let header_group: SharedString = format!("turn-{index}").into();
            list = list.child(
                div()
                    .id(("turn", index))
                    .group(header_group.clone())
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .pt_2()
                    .pb_0p5()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if group.is_restore {
                                colors.text_muted
                            } else {
                                colors.text
                            })
                            .child(group.label.clone()),
                    )
                    .when_some(group.status.label(), |header, label| {
                        header.child(div().flex_shrink_0().text_size(px(11.)).text_color(colors.text_muted).child(label))
                    })
                    .when(!running, |this| {
                        this.child(
                            div()
                                .flex()
                                .gap_0p5()
                                .invisible()
                                .group_hover_style(header_group, |s| s.visible())
                                .when(group.status != RevertStatus::Reverted, |actions| actions.child(
                                    small_button(
                                        ("revert-turn", index),
                                        if group.is_restore { "Undo" } else { "Revert" },
                                        colors.error,
                                        cx,
                                    )
                                    .tooltip(tooltip(if group.is_restore {
                                        "Undo this restore"
                                    } else {
                                        "Undo the changes made in this turn (later changes are kept)"
                                    }))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.revert(Scope::Group(index), None, window, cx)
                                        },
                                    )),
                                ))
                                .when(!group.is_restore, |this| {
                                    this.child(
                                        small_button(
                                            ("restore-checkpoint", index),
                                            "Restore",
                                            colors.accent,
                                            cx,
                                        )
                                        .tooltip(tooltip(
                                            "Restore the checkpoint before this turn: every file changed since goes back",
                                        ))
                                        .on_click(cx.listener(
                                            move |this, _, window, cx| {
                                                this.restore_checkpoint(index, window, cx)
                                            },
                                        )),
                                    )
                                }),
                        )
                    }),
            );
            for (file_index, row) in group.files.iter().enumerate() {
                let open_path = row.path.clone();
                let revert_path = row.path.clone();
                let on_revert: Option<PanelAction> = (!running).then(|| {
                    Box::new(
                        move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                            this.revert(Scope::Group(index), Some(revert_path.clone()), window, cx)
                        },
                    ) as Box<_>
                });
                list = list.child(self.render_file(
                    format!("turn-{index}-file-{file_index}").into(),
                    row,
                    move |this, _, cx| this.open_file(Scope::Group(index), &open_path, cx),
                    on_revert,
                    cx,
                ));
            }
        }
        list.into_any_element()
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let running = self.running(cx) && self.mode != ChangesMode::Git;
        let body = match self.mode {
            ChangesMode::Session => self.render_session(cx),
            ChangesMode::Turns => self.render_turns(cx),
            // The Git view scrolls its file list and keeps its commit box in view. It is
            // drawn again only when it changes, not on every frame of the window (e.g.
            // while the conversation animates). Not in tests: gpui forgets the debug
            // selectors of a reused view, which tests look elements up by.
            ChangesMode::Git if cfg!(test) => self.git.clone().into_any_element(),
            ChangesMode::Git => self
                .git
                .clone()
                .cached(StyleRefinement::default().size_full())
                .into_any_element(),
        };
        let git = self.mode == ChangesMode::Git;
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(cx))
            .when(running, |this| {
                this.child(
                    div()
                        .px_3()
                        .pb_1()
                        .text_size(px(11.))
                        .text_color(colors.text_muted)
                        .child("The agent is working; reverting is available when it stops."),
                )
            })
            .child(
                div()
                    .id("changes-list")
                    .flex_1()
                    .min_h_0()
                    .when(git, |this| this.flex().flex_col())
                    .when(!git, |this| this.pb_2().overflow_y_scroll())
                    .child(body),
            )
    }
}

fn empty(text: &str, cx: &App) -> AnyElement {
    div()
        .px_3()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().colors.text_muted)
        .child(text.to_owned())
        .into_any_element()
}

fn stats(added: usize, removed: usize, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .flex_shrink_0()
        .gap_1()
        .text_size(px(11.))
        .child(div().text_color(colors.success).child(format!("+{added}")))
        .child(div().text_color(colors.error).child(format!("−{removed}")))
}

fn small_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    hover: Rgba,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = &cx.theme().colors;
    let background = colors.background;
    div()
        .id(id)
        .flex_shrink_0()
        .px_1()
        .rounded(px(3.))
        .cursor_pointer()
        .text_size(px(11.))
        .text_color(colors.text_muted)
        .hover_style(move |s| s.text_color(hover).bg(background))
        .child(label)
}

fn kind_badge(kind: FileKind, cx: &App) -> (&'static str, Rgba) {
    let colors = &cx.theme().colors;
    match kind {
        FileKind::Added => ("A", colors.success),
        FileKind::Modified => ("M", colors.warning),
        FileKind::Deleted => ("D", colors.error),
    }
}

/// `src/a/b.rs` → (`b.rs`, `src/a`).
fn split_path(display: &str) -> (String, String) {
    match display.rsplit_once('/') {
        Some((dir, name)) => (name.to_owned(), dir.to_owned()),
        None => (display.to_owned(), String::new()),
    }
}
