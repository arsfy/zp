//! The composer's extras: `/` commands (built-in, prompt templates, skills), `@file`
//! completion, image attachments (paste and drop), and compaction (manual and
//! automatic).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, ClipboardEntry, Context, ExternalPaths, Window};
use zp_core::attachments::{ImageAttachment, fuzzy_filter, image_mime};

use super::{ConversationEvent, ConversationView, Picker};

/// Items shown in a completion menu.
const MENU_ITEMS: usize = 50;
/// Files listed for `@` completion.
const FILE_LIMIT: usize = 20_000;

/// A built-in `/` command.
pub struct BuiltinCommand {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
}

pub const BUILTIN_COMMANDS: [BuiltinCommand; 10] = [
    BuiltinCommand {
        name: "compact",
        args: "[focus]",
        description: "Summarize older messages to free up context",
    },
    BuiltinCommand {
        name: "new",
        args: "",
        description: "Start a new conversation in a new tab",
    },
    BuiltinCommand {
        name: "model",
        args: "",
        description: "Choose the model",
    },
    BuiltinCommand {
        name: "thinking",
        args: "",
        description: "Choose the reasoning level",
    },
    BuiltinCommand {
        name: "permissions",
        args: "",
        description: "Choose the permission mode",
    },
    BuiltinCommand {
        name: "tree",
        args: "",
        description: "Show the session tree",
    },
    BuiltinCommand {
        name: "mcp",
        args: "",
        description: "Show MCP servers",
    },
    BuiltinCommand {
        name: "reload",
        args: "",
        description: "Reload prompt templates, skills and MCP servers",
    },
    BuiltinCommand {
        name: "settings",
        args: "",
        description: "Open the settings",
    },
    BuiltinCommand {
        name: "help",
        args: "",
        description: "List the commands",
    },
];

/// What a completion inserts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    /// The whole draft after accepting.
    pub replacement: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    File,
}

pub struct Completion {
    pub kind: CompletionKind,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
}

/// Commands the app runs itself (forwarded to the window).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowCommand {
    NewChat,
    Settings,
    Mcp,
    Reload,
}

impl ConversationView {
    /// The open completion menu (tests).
    #[cfg(test)]
    pub fn completion(&self) -> Option<&Completion> {
        self.completion.as_ref()
    }

    /// Recomputes the completion menu for the current draft.
    pub(super) fn update_completion(&mut self, cx: &mut Context<Self>) {
        let draft = self.composer.read(cx).as_str().to_owned();
        let next = self.completion_for(&draft, cx);
        let selected = match (&self.completion, &next) {
            (Some(old), Some((kind, items))) if old.kind == *kind => {
                old.selected.min(items.len().saturating_sub(1))
            }
            _ => 0,
        };
        let changed = match (&self.completion, &next) {
            (None, None) => false,
            (Some(old), Some((kind, items))) => old.kind != *kind || old.items != *items,
            _ => true,
        };
        self.completion = next.map(|(kind, items)| Completion {
            kind,
            items,
            selected,
        });
        if changed {
            cx.notify();
        }
    }

    fn completion_for(
        &mut self,
        draft: &str,
        cx: &mut Context<Self>,
    ) -> Option<(CompletionKind, Vec<CompletionItem>)> {
        if let Some(query) = draft.strip_prefix('/')
            && !query.contains(char::is_whitespace)
        {
            let items: Vec<CompletionItem> = self
                .command_items(cx)
                .into_iter()
                .filter(|(name, _, _)| {
                    name.starts_with(query) || (query.len() >= 3 && name.contains(query))
                })
                .take(MENU_ITEMS)
                .map(|(name, args, detail)| CompletionItem {
                    label: if args.is_empty() {
                        format!("/{name}")
                    } else {
                        format!("/{name} {args}")
                    },
                    detail,
                    replacement: format!("/{name} "),
                })
                .collect();
            return (!items.is_empty()).then_some((CompletionKind::Command, items));
        }
        if draft.ends_with(char::is_whitespace) {
            return None;
        }
        let start = draft.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let token = &draft[start..];
        let query = token.strip_prefix('@')?;
        let files = self.project_files(cx)?;
        let items: Vec<CompletionItem> = fuzzy_filter(&files, query, MENU_ITEMS)
            .into_iter()
            .map(|file| CompletionItem {
                label: file.to_owned(),
                detail: String::new(),
                replacement: format!("{}@{file} ", &draft[..start]),
            })
            .collect();
        (!items.is_empty()).then_some((CompletionKind::File, items))
    }

    /// Built-in commands, prompt templates and skills: (name, argument hint, description).
    fn command_items(&self, _cx: &App) -> Vec<(String, String, String)> {
        let mut items: Vec<(String, String, String)> = BUILTIN_COMMANDS
            .iter()
            .map(|c| {
                (
                    c.name.to_owned(),
                    c.args.to_owned(),
                    c.description.to_owned(),
                )
            })
            .collect();
        if let Some(coding) = &self.coding {
            let resources = coding.resources();
            for template in &resources.templates {
                if !items.iter().any(|(name, _, _)| *name == template.name) {
                    items.push((
                        template.name.clone(),
                        template.argument_hint.clone().unwrap_or_default(),
                        template.description.clone(),
                    ));
                }
            }
            for skill in &resources.skills {
                items.push((
                    format!("skill:{}", skill.name),
                    String::new(),
                    skill.description.clone(),
                ));
            }
        }
        items
    }

    /// The project's files, loaded in the background on first use.
    fn project_files(&mut self, cx: &mut Context<Self>) -> Option<Arc<Vec<String>>> {
        if let Some(files) = &self.file_list {
            return Some(files.clone());
        }
        if !self.loading_files {
            self.loading_files = true;
            let root = self.root.clone();
            let task = cx
                .background_executor()
                .spawn(async move { zp_core::attachments::project_files(&root, FILE_LIMIT) });
            cx.spawn(async move |this, cx| {
                let files = Arc::new(task.await);
                let _ = this.update(cx, |this, cx| {
                    this.file_list = Some(files);
                    this.loading_files = false;
                    this.update_completion(cx);
                });
            })
            .detach();
        }
        None
    }

    /// Forgets the cached file list (files may have changed).
    pub(super) fn invalidate_files(&mut self) {
        self.file_list = None;
    }

    pub(crate) fn move_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(completion) = &mut self.completion {
            let count = completion.items.len() as isize;
            completion.selected = (completion.selected as isize + delta).rem_euclid(count) as usize;
            cx.notify();
        }
    }

    /// Accepts item `index` (or the selected one).
    pub(crate) fn accept_completion(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(completion) = self.completion.take() else {
            return false;
        };
        let Some(item) = completion.items.get(index.unwrap_or(completion.selected)) else {
            return false;
        };
        let text = item.replacement.clone();
        self.composer.update(cx, |composer, cx| {
            composer.emplace(&text, cx);
            composer.move_to(text.len(), cx);
        });
        self.focus_composer(window, cx);
        self.update_completion(cx);
        cx.notify();
        true
    }

    pub(crate) fn close_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            self.suppress_completion = Some(self.composer.read(cx).as_str().to_owned());
            cx.notify();
        }
    }

    // ----- commands ----------------------------------------------------------------

    /// Runs `input` if it is a built-in command; returns whether it was one.
    pub(super) fn run_builtin(
        &mut self,
        input: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(command) = input.strip_prefix('/') else {
            return false;
        };
        let (name, args) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(n, a)| (n, a.trim()));
        match name {
            "compact" => self.compact(Some(args.to_owned()).filter(|a| !a.is_empty()), cx),
            "new" => cx.emit(ConversationEvent::Command(WindowCommand::NewChat)),
            "model" => self.toggle_picker(Picker::Model, window, cx),
            "thinking" => self.toggle_picker(Picker::Thinking, window, cx),
            "permissions" => self.toggle_picker(Picker::Permissions, window, cx),
            "tree" => self.toggle_tree(cx),
            "mcp" => cx.emit(ConversationEvent::Command(WindowCommand::Mcp)),
            "reload" => cx.emit(ConversationEvent::Command(WindowCommand::Reload)),
            "settings" => cx.emit(ConversationEvent::Command(WindowCommand::Settings)),
            "help" => {
                let mut lines: Vec<String> = BUILTIN_COMMANDS
                    .iter()
                    .map(|c| {
                        format!("/{} {} — {}", c.name, c.args, c.description).replace("  ", " ")
                    })
                    .collect();
                let names: Vec<String> = self
                    .command_items(cx)
                    .into_iter()
                    .skip(BUILTIN_COMMANDS.len())
                    .map(|(name, _, _)| format!("/{name}"))
                    .collect();
                if !names.is_empty() {
                    lines.push(format!("Templates and skills: {}", names.join(", ")));
                }
                lines.push("Type @ to attach a file; paste or drop images to attach them.".into());
                self.info = Some(lines.join("\n"));
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    // ----- attachments -------------------------------------------------------------

    /// Images attached to the next message (tests).
    #[cfg(test)]
    pub fn attachments(&self) -> &[ImageAttachment] {
        &self.attachments
    }

    pub(crate) fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
            cx.notify();
        }
    }

    /// `cmd-v` with an image (or copied files) on the clipboard attaches them; returns
    /// whether the clipboard had any (plain text is left to the text field).
    pub(crate) fn paste_attachments(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let mut handled = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let mime = image.format.mime_type().to_owned();
                    if image_mime(Path::new(&format!(
                        "x.{}",
                        mime.rsplit('/').next().unwrap_or("")
                    )))
                    .is_some()
                    {
                        self.attachments.push(ImageAttachment {
                            name: "Pasted image".into(),
                            mime_type: mime,
                            data: image.bytes.clone(),
                        });
                    } else {
                        self.info = Some(format!("Images of type {mime} cannot be attached"));
                    }
                    handled = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.attach_paths(paths.paths(), cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if handled {
            cx.notify();
        }
        handled
    }

    /// Dropped files: images are attached, other files referenced with `@`.
    pub(crate) fn drop_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach_paths(paths.paths(), cx);
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn attach_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let mut references = Vec::new();
        for path in paths {
            if image_mime(path).is_some() {
                match ImageAttachment::from_file(path) {
                    Ok(image) => self.attachments.push(image),
                    Err(error) => self.info = Some(format!("{error:#}")),
                }
            } else if path.is_file() {
                let shown = zp_tools::display_path(&self.root, path);
                references.push(if shown.contains(' ') {
                    format!("@\"{shown}\"")
                } else {
                    format!("@{shown}")
                });
            }
        }
        if !references.is_empty() {
            let draft = self.composer.read(cx).as_str().to_owned();
            let separator = if draft.is_empty() || draft.ends_with(char::is_whitespace) {
                ""
            } else {
                " "
            };
            let text = format!("{draft}{separator}{} ", references.join(" "));
            self.composer.update(cx, |composer, cx| {
                composer.emplace(&text, cx);
                composer.move_to(text.len(), cx);
            });
        }
    }

    // ----- compaction --------------------------------------------------------------

    /// The note above the composer (tests).
    #[cfg(test)]
    pub fn info_text(&self) -> Option<String> {
        self.info.clone()
    }

    /// Compactions on the shown branch (tests).
    #[cfg(test)]
    pub fn compaction_count(&self) -> usize {
        self.coding.as_ref().map_or(0, |coding| {
            coding
                .session()
                .branch_path(None)
                .iter()
                .filter(|e| matches!(e.data, zp_session::EntryData::Compaction { .. }))
                .count()
        })
    }

    /// Whether a compaction is running.
    pub fn is_compacting(&self) -> bool {
        self.compacting
    }

    /// Summarizes older messages; then runs `then` (a prompt or a retry) if given.
    pub(crate) fn compact(&mut self, focus: Option<String>, cx: &mut Context<Self>) {
        self.compact_then(focus, None, cx);
    }

    pub(super) fn compact_then(
        &mut self,
        focus: Option<String>,
        then: Option<super::Followup>,
        cx: &mut Context<Self>,
    ) {
        let Some(coding) = self.coding.clone() else {
            return;
        };
        if self.running || self.compacting {
            return;
        }
        self.compacting = true;
        self.run_error = None;
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
        let task =
            gpui_tokio::Tokio::spawn(cx, async move { coding.compact(focus.as_deref()).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.compacting = false;
                match result {
                    Ok(Ok(Some((before, after)))) => {
                        this.info = Some(format!(
                            "Compacted the conversation: about {} → {} tokens",
                            crate::ui::format_tokens(before),
                            crate::ui::format_tokens(after)
                        ));
                        this.reload_transcript();
                    }
                    Ok(Ok(None)) => this.info = Some("Nothing to compact yet".into()),
                    Ok(Err(error)) => {
                        this.run_error = Some(format!("Compaction failed: {error:#}"))
                    }
                    Err(error) => this.run_error = Some(format!("Compaction failed: {error}")),
                }
                cx.emit(ConversationEvent::StatusChanged);
                cx.emit(ConversationEvent::FilesChanged);
                if let Some(then) = then
                    && this.run_error.is_none()
                {
                    this.run_followup(then, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_are_unique() {
        let mut names: Vec<&str> = BUILTIN_COMMANDS.iter().map(|c| c.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), BUILTIN_COMMANDS.len());
    }
}
