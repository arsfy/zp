//! The composer's extras: `/` commands (built-in, prompt templates, skills), `@file`
//! completion, image attachments (paste and drop), and compaction (manual and
//! automatic).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Bounds, ClipboardEntry, Context, EntityInputHandler as _, ExternalPaths, Window, px,
};
use zp_ai::ThinkingLevel;
use zp_core::attachments::{ImageAttachment, fuzzy_filter, image_mime};
use zp_core::permissions::PermissionMode;

use super::{ConversationEvent, ConversationView};

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
        args: "[model]",
        description: "Choose the model",
    },
    BuiltinCommand {
        name: "thinking",
        args: "[level]",
        description: "Choose the reasoning level",
    },
    BuiltinCommand {
        name: "permissions",
        args: "[mode]",
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

/// Built-in commands whose argument is chosen from a list in the completion menu.
const OPTION_COMMANDS: [&str; 3] = ["model", "thinking", "permissions"];

/// What accepting a completion does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionAction {
    /// Replaces the whole draft.
    Insert(String),
    /// Switches to the model with this qualified id.
    Model(String),
    Thinking(ThinkingLevel),
    Permissions(PermissionMode),
}

/// One completion menu entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub action: CompletionAction,
    /// The current choice (marked in the menu).
    pub current: bool,
}

impl CompletionItem {
    fn insert(label: String, detail: String, replacement: String) -> Self {
        Self {
            label,
            detail,
            action: CompletionAction::Insert(replacement),
            current: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    File,
    /// The choices of `/model`, `/thinking` or `/permissions`.
    Option,
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
    pub(super) fn remember_prompt(&mut self, text: &str) {
        if !text.is_empty() && self.prompt_history.last().is_none_or(|last| last != text) {
            self.prompt_history.push(text.to_owned());
        }
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Returns whether a history entry consumed the vertical navigation action.
    pub(super) fn recall_prompt(
        &mut self,
        previous: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.completion.is_some()
            || self.prompt_history.is_empty()
            || (!previous && self.history_index.is_none())
        {
            return false;
        }
        let can_recall = self.composer.update(cx, |input, cx| {
            if input.marked_text_range(window, cx).is_some() {
                return false;
            }
            let Some(selection) = input.selected_text_range(false, window, cx) else {
                return false;
            };
            if !selection.range.is_empty() {
                return false;
            }
            if self.history_index.is_some() {
                return true;
            }
            if input
                .as_str()
                .encode_utf16()
                .take(selection.range.start)
                .any(|unit| unit == u16::from(b'\n'))
            {
                return false;
            }
            let caret =
                input.bounds_for_range(selection.range.clone(), Bounds::default(), window, cx);
            let boundary = input.bounds_for_range(0..0, Bounds::default(), window, cx);
            match (caret, boundary) {
                (Some(caret), Some(boundary)) => (caret.top() - boundary.top()).abs() < px(1.),
                _ => selection.range.start == 0 || !input.as_str().contains('\n'),
            }
        });
        if !can_recall {
            return false;
        }
        if self.history_index.is_none() {
            self.history_draft = self.composer.read(cx).as_str().to_owned();
        }
        let index = match (self.history_index, previous) {
            (None, _) => self.prompt_history.len() - 1,
            (Some(index), true) => index.saturating_sub(1),
            (Some(index), false) => index + 1,
        };
        let text = if index == self.prompt_history.len() {
            self.history_index = None;
            std::mem::take(&mut self.history_draft)
        } else {
            self.history_index = Some(index);
            self.prompt_history[index].clone()
        };
        // Recalling a slash command must leave arrows available for history navigation.
        self.completion = None;
        self.suppress_completion = Some(text.clone());
        self.composer.update(cx, |input, cx| {
            input.emplace(&text, cx);
            input.move_to(text.len(), cx);
        });
        cx.notify();
        true
    }

    /// The open completion menu (tests).
    #[cfg(test)]
    pub fn completion(&self) -> Option<&Completion> {
        self.completion.as_ref()
    }

    /// Recomputes the completion menu for the current draft.
    pub(super) fn update_completion(&mut self, cx: &mut Context<Self>) {
        let draft = self.composer.read(cx).as_str().to_owned();
        let next = self.completion_for(&draft, cx);
        let changed = match (&self.completion, &next) {
            (None, None) => false,
            (Some(old), Some((kind, items))) => old.kind != *kind || old.items != *items,
            _ => true,
        };
        if !changed {
            return;
        }
        // A newly opened list starts at the current choice (if any); a refined one at
        // the best match.
        let reopened = match (&self.completion, &next) {
            (Some(old), Some((kind, _))) => old.kind != *kind,
            _ => true,
        };
        let selected = next.as_ref().map_or(0, |(_, items)| {
            reopened
                .then(|| items.iter().position(|item| item.current))
                .flatten()
                .unwrap_or(0)
        });
        self.completion = next.map(|(kind, items)| Completion {
            kind,
            items,
            selected,
        });
        self.completion_scroll.scroll_to_item(selected);
        cx.notify();
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
                .map(|(name, args, detail)| {
                    let label = if args.is_empty() {
                        format!("/{name}")
                    } else {
                        format!("/{name} {args}")
                    };
                    CompletionItem::insert(label, detail, format!("/{name} "))
                })
                .collect();
            return (!items.is_empty()).then_some((CompletionKind::Command, items));
        }
        if let Some((name, query)) = draft
            .strip_prefix('/')
            .and_then(|command| command.split_once(char::is_whitespace))
            && OPTION_COMMANDS.contains(&name)
        {
            let items = self.option_items(name, query.trim(), cx);
            return (!items.is_empty()).then_some((CompletionKind::Option, items));
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
            .map(|file| {
                CompletionItem::insert(
                    file.to_owned(),
                    String::new(),
                    format!("{}@{file} ", &draft[..start]),
                )
            })
            .collect();
        (!items.is_empty()).then_some((CompletionKind::File, items))
    }

    /// The choices of option command `name` matching `query`.
    fn option_items(&self, name: &str, query: &str, cx: &App) -> Vec<CompletionItem> {
        if self.coding.is_none() {
            return Vec::new();
        }
        let query = query.to_lowercase();
        match name {
            "model" => {
                let current = self.model().map(|model| model.qualified_id());
                self.models_matching(&query, cx)
                    .into_iter()
                    .take(MENU_ITEMS)
                    .map(|choice| {
                        let id = choice.model.qualified_id();
                        let context = choice.model.context_window;
                        CompletionItem {
                            current: current.as_ref() == Some(&id),
                            label: id.clone(),
                            detail: if context >= 1000 {
                                format!("{} · {}k", choice.provider_name, context / 1000)
                            } else {
                                choice.provider_name
                            },
                            action: CompletionAction::Model(id),
                        }
                    })
                    .collect()
            }
            "thinking" => {
                let current = self.effective_thinking();
                self.thinking_levels()
                    .into_iter()
                    .filter(|level| level.as_str().contains(&query))
                    .map(|level| CompletionItem {
                        label: level.as_str().to_owned(),
                        detail: String::new(),
                        action: CompletionAction::Thinking(level),
                        current: level == current,
                    })
                    .collect()
            }
            "permissions" => {
                let current = self.permission_mode();
                PermissionMode::ALL
                    .into_iter()
                    .filter(|mode| {
                        mode.cli_name().contains(&query)
                            || mode.label().to_lowercase().contains(&query)
                    })
                    .map(|mode| CompletionItem {
                        label: mode.cli_name().to_owned(),
                        detail: format!("{} — {}", mode.label(), mode.description()),
                        action: CompletionAction::Permissions(mode),
                        current: mode == current,
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// For `/model`, `/thinking` or `/permissions` without an argument: lists the
    /// choices in the completion menu. Returns whether `input` was such a command.
    pub(super) fn open_command_options(
        &mut self,
        input: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(name) = input
            .strip_prefix('/')
            .filter(|name| OPTION_COMMANDS.contains(name))
        else {
            return false;
        };
        let text = format!("/{name} ");
        self.suppress_completion = None;
        self.composer.update(cx, |composer, cx| {
            composer.emplace(&text, cx);
            composer.move_to(text.len(), cx);
        });
        self.focus_composer(window, cx);
        self.update_completion(cx);
        if self.completion.is_none() && name == "model" {
            self.info = Some("No available models; add a provider in Settings".into());
        }
        cx.notify();
        true
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
            self.completion_scroll.scroll_to_item(completion.selected);
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
        let text = match item.action.clone() {
            CompletionAction::Insert(text) => text,
            action => {
                self.composer
                    .update(cx, |composer, cx| composer.emplace("", cx));
                self.info = None;
                match action {
                    CompletionAction::Model(id) => self.select_model_id(&id, window, cx),
                    CompletionAction::Thinking(level) => self.select_thinking(level, window, cx),
                    CompletionAction::Permissions(mode) => {
                        self.select_permission_mode(mode, window, cx)
                    }
                    CompletionAction::Insert(_) => unreachable!(),
                }
                return true;
            }
        };
        self.composer.update(cx, |composer, cx| {
            composer.emplace(&text, cx);
            composer.move_to(text.len(), cx);
        });
        self.focus_composer(window, cx);
        self.update_completion(cx);
        cx.notify();
        true
    }

    /// Switches to the available model with qualified id `id`.
    fn select_model_id(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self
            .model_choices(cx)
            .into_iter()
            .find(|choice| choice.model.qualified_id() == id)
        {
            Some(choice) => self.select_model(choice.model, window, cx),
            None => {
                self.info = Some(format!("Model {id} is not available"));
                cx.notify();
            }
        }
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
            "model" => self.run_model_command(args, window, cx),
            "thinking" => self.run_thinking_command(args, window, cx),
            "permissions" => self.run_permissions_command(args, window, cx),
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

    /// `/model <query>`: switches to the only model matching `query` (or an exact id).
    fn run_model_command(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let choices = self.models_matching(query, cx);
        let lower = query.to_lowercase();
        let exact = choices.iter().find(|choice| {
            choice.model.qualified_id().to_lowercase() == lower
                || choice.model.id.to_lowercase() == lower
        });
        match (exact, choices.as_slice()) {
            (Some(choice), _) | (None, [choice]) => {
                self.select_model(choice.model.clone(), window, cx)
            }
            (None, []) => self.info = Some(format!("No available model matches “{query}”")),
            (None, _) => {
                self.info = Some(format!(
                    "{} models match “{query}”; type /model to choose one",
                    choices.len()
                ))
            }
        }
    }

    /// `/thinking <level>`.
    fn run_thinking_command(&mut self, arg: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.coding.is_none() {
            return;
        }
        let supported = self.thinking_levels();
        match arg.to_lowercase().parse::<ThinkingLevel>() {
            Ok(level) if supported.contains(&level) => self.select_thinking(level, window, cx),
            _ => {
                let names: Vec<&str> = supported.iter().map(|l| l.as_str()).collect();
                self.info = Some(format!(
                    "Unknown reasoning level “{arg}”; this model supports: {}",
                    names.join(", ")
                ));
            }
        }
    }

    /// `/permissions <mode>`.
    fn run_permissions_command(&mut self, arg: &str, window: &mut Window, cx: &mut Context<Self>) {
        let lower = arg.to_lowercase();
        match PermissionMode::ALL
            .into_iter()
            .find(|mode| mode.cli_name() == lower || mode.label().to_lowercase() == lower)
        {
            Some(mode) => self.select_permission_mode(mode, window, cx),
            None => {
                let names: Vec<&str> = PermissionMode::ALL.iter().map(|m| m.cli_name()).collect();
                self.info = Some(format!(
                    "Unknown permission mode “{arg}”; choose one of: {}",
                    names.join(", ")
                ));
            }
        }
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
        self.start_status_updates(cx);
        self.refresh_last_row();
        self.run_error = None;
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
        let task =
            gpui_tokio::Tokio::spawn(cx, async move { coding.compact(focus.as_deref()).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.compacting = false;
                this.refresh_last_row();
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
                } else {
                    this.stop_status_updates(cx);
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
