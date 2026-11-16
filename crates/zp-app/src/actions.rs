//! Application-wide actions, key bindings and menus.
//!
//! Key bindings use `secondary`, which is cmd on macOS and ctrl on Linux / Windows.

use gpui::{App, KeyBinding, Menu, MenuItem, PromptLevel, actions};
use gpui_elements::editable_text::actions::{DEFAULT_INPUT_CONTEXT, Enter, default_bindings};

use crate::workspace;

actions!(
    zp,
    [
        About,
        Quit,
        OpenFolder,
        NewWindow,
        CloseTab,
        CloseWindow,
        ToggleSidebar,
        ToggleRightPanel,
        NewChat,
        SendMessage,
        StopRun,
        ClosePicker,
        PickFirstModel,
        NextTab,
        PreviousTab,
        ToggleSessionTree,
        SelectNextSession,
        SelectPreviousSession,
        OpenSelectedSession,
        ClearSessionSearch,
        ConfirmRename,
        CancelRename,
        DismissMenu,
        CloseDiff,
        NextDiffFile,
        PreviousDiffFile,
        ToggleDiffLayout,
        DenyToolCall,
        OpenSettings,
        CancelRecording,
        CompletionNext,
        CompletionPrevious,
        AcceptCompletion,
        CloseCompletion,
        CommandPalette,
        CompactConversation,
        CheckForUpdates,
        ReloadResources,
        SubmitEdit,
        CancelEdit,
        ToggleTerminal,
        NewTerminal
    ]
);

/// Activates the tab at an index (`usize::MAX` for the last tab).
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = zp, no_json)]
pub struct ActivateTab(pub usize);

/// Key context of project windows.
pub const PROJECT_CONTEXT: &str = "ProjectWindow";

pub const KEY_SEND: &str = "enter";

/// The composer's text field.
const COMPOSER_INPUT: &str = "Composer > EditableText";
/// The field editing a sent message.
const MESSAGE_EDITOR_INPUT: &str = "MessageEditor > EditableText";
/// The model picker's filter field.
const MODEL_FILTER_INPUT: &str = "ModelPicker > EditableText";
/// The sidebar's session search field.
const SESSION_SEARCH_INPUT: &str = "SessionSearch > EditableText";
/// The composer while its completion menu is open.
const COMPLETION_INPUT: &str = "CompletionMenu > EditableText";
/// The command palette's filter field.
const PALETTE_INPUT: &str = "CommandPalette > EditableText";
/// The rename field.
const RENAME_INPUT: &str = "RenameInput > EditableText";

/// A shortcut the user can change in the settings.
pub struct Shortcut {
    /// Shown in the settings.
    pub label: &'static str,
    /// The action's name, e.g. `zp::NewChat` (the key in `settings.keybindings`).
    pub action: &'static str,
    pub default: &'static str,
    pub context: Option<&'static str>,
    bind: fn(&str, Option<&str>) -> KeyBinding,
}

macro_rules! shortcut {
    ($label:expr, $action:ident, $default:expr, $context:expr) => {
        Shortcut {
            label: $label,
            action: concat!("zp::", stringify!($action)),
            default: $default,
            context: $context,
            bind: |keys, context| KeyBinding::new(keys, $action, context),
        }
    };
}

/// The customizable shortcuts, in the order the settings list them.
pub fn shortcuts() -> Vec<Shortcut> {
    let project = Some(PROJECT_CONTEXT);
    vec![
        shortcut!("Command Palette", CommandPalette, "secondary-shift-p", None),
        shortcut!("New Chat", NewChat, "secondary-t", project),
        shortcut!("Compact Conversation", CompactConversation, "", project),
        shortcut!("Send Message", SendMessage, KEY_SEND, Some(COMPOSER_INPUT)),
        shortcut!("Close Tab", CloseTab, "secondary-w", None),
        shortcut!("Next Tab", NextTab, "ctrl-tab", project),
        shortcut!("Previous Tab", PreviousTab, "ctrl-shift-tab", project),
        shortcut!(
            "Session Tree",
            ToggleSessionTree,
            "secondary-shift-t",
            project
        ),
        shortcut!("Toggle Sidebar", ToggleSidebar, "secondary-b", project),
        shortcut!("Toggle Terminal", ToggleTerminal, "ctrl-`", project),
        shortcut!("New Terminal", NewTerminal, "ctrl-shift-`", project),
        shortcut!(
            "Toggle Changes Panel",
            ToggleRightPanel,
            "secondary-alt-b",
            project
        ),
        shortcut!(
            "Toggle Diff Layout",
            ToggleDiffLayout,
            "secondary-alt-d",
            Some("DiffView")
        ),
        shortcut!("Open Folder", OpenFolder, "secondary-o", None),
        shortcut!("New Window", NewWindow, "secondary-shift-n", None),
        shortcut!("Close Window", CloseWindow, "secondary-shift-w", None),
        shortcut!("Settings", OpenSettings, "secondary-,", None),
        shortcut!("Quit", Quit, "secondary-q", None),
    ]
}

/// Whether every keystroke of `keys` parses.
pub fn valid_keystrokes(keys: &str) -> bool {
    !keys.trim().is_empty()
        && keys
            .split_whitespace()
            .all(|key| gpui::Keystroke::parse(key).is_ok())
}

/// The keystrokes of shortcut `action` with the user's override (`None`: unbound).
pub fn shortcut_keys(action: &str, cx: &App) -> Option<String> {
    let shortcut = shortcuts().into_iter().find(|s| s.action == action)?;
    let keys = cx
        .try_global::<crate::store::AppSettings>()
        .and_then(|_| {
            crate::store::AppSettings::get(cx)
                .keybindings
                .get(action)
                .cloned()
        })
        .unwrap_or_else(|| shortcut.default.to_owned());
    valid_keystrokes(&keys).then_some(keys)
}

/// Registers global action handlers, key bindings and the application menu.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| workspace::quit(cx));
    cx.on_action(show_about);
    cx.on_action(|_: &OpenSettings, cx| crate::settings::SettingsWindow::open(cx));
    // Welcome windows handle this themselves so the project can replace them.
    cx.on_action(|_: &OpenFolder, cx| workspace::prompt_open_folder(None, cx));
    cx.on_action(|_: &NewWindow, cx| workspace::open_welcome_or_report(cx));
    cx.on_action(|_: &CloseWindow, cx| workspace::close_active_window(cx));
    // Without a project window (welcome, settings), closing "the tab" closes the window.
    cx.on_action(|_: &CloseTab, cx| workspace::close_active_window(cx));
    bind_keys(cx);

    // Quit once the last window is closed, on every platform.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            workspace::quit(cx);
        }
    })
    .detach();
}

/// (Re)binds every key, with the user's shortcut overrides, and rebuilds the menus.
pub fn bind_keys(cx: &mut App) {
    cx.clear_key_bindings();
    // `secondary-1` … `secondary-8` pick a tab, `secondary-9` the last one; the platform
    // shortcuts for the next / previous tab work too.
    for index in 0..9 {
        let target = if index == 8 { usize::MAX } else { index };
        cx.bind_keys([KeyBinding::new(
            &format!("secondary-{}", index + 1),
            ActivateTab(target),
            Some(PROJECT_CONTEXT),
        )]);
    }
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-shift-]", NextTab, Some(PROJECT_CONTEXT)),
            KeyBinding::new("cmd-shift-[", PreviousTab, Some(PROJECT_CONTEXT)),
        ]);
    } else {
        cx.bind_keys([
            KeyBinding::new("ctrl-pagedown", NextTab, Some(PROJECT_CONTEXT)),
            KeyBinding::new("ctrl-pageup", PreviousTab, Some(PROJECT_CONTEXT)),
        ]);
    }
    // Text fields first, so the more specific bindings below override them.
    cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
    cx.bind_keys([
        KeyBinding::new(
            "secondary-c",
            gpui_elements::editable_text::actions::Copy,
            Some("Conversation"),
        ),
        KeyBinding::new(
            "secondary-a",
            gpui_elements::editable_text::actions::SelectAll,
            Some("Conversation"),
        ),
        KeyBinding::new("shift-enter", Enter, Some(COMPOSER_INPUT)),
        KeyBinding::new("enter", SubmitEdit, Some(MESSAGE_EDITOR_INPUT)),
        KeyBinding::new("shift-enter", Enter, Some(MESSAGE_EDITOR_INPUT)),
        KeyBinding::new("escape", CancelEdit, Some(MESSAGE_EDITOR_INPUT)),
        KeyBinding::new("escape", StopRun, Some(COMPOSER_INPUT)),
        KeyBinding::new("enter", PickFirstModel, Some(MODEL_FILTER_INPUT)),
        KeyBinding::new("escape", ClosePicker, Some(MODEL_FILTER_INPUT)),
        KeyBinding::new("down", SelectNextSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("up", SelectPreviousSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("enter", OpenSelectedSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("escape", ClearSessionSearch, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("enter", ConfirmRename, Some(RENAME_INPUT)),
        KeyBinding::new("escape", CancelRename, Some(RENAME_INPUT)),
        KeyBinding::new("escape", DismissMenu, Some("ContextMenu")),
        KeyBinding::new("escape", CloseDiff, Some("DiffView")),
        KeyBinding::new("enter", DenyToolCall, Some("ApprovalReason > EditableText")),
        KeyBinding::new("]", NextDiffFile, Some("DiffView")),
        KeyBinding::new("[", PreviousDiffFile, Some("DiffView")),
        KeyBinding::new("escape", CancelRecording, Some("Keybindings")),
        KeyBinding::new("down", CompletionNext, Some(COMPLETION_INPUT)),
        KeyBinding::new("up", CompletionPrevious, Some(COMPLETION_INPUT)),
        KeyBinding::new("tab", AcceptCompletion, Some(COMPLETION_INPUT)),
        KeyBinding::new("escape", CloseCompletion, Some(COMPLETION_INPUT)),
        KeyBinding::new("down", CompletionNext, Some(PALETTE_INPUT)),
        KeyBinding::new("up", CompletionPrevious, Some(PALETTE_INPUT)),
        KeyBinding::new("enter", AcceptCompletion, Some(PALETTE_INPUT)),
        KeyBinding::new("escape", CloseCompletion, Some(PALETTE_INPUT)),
        KeyBinding::new("escape", CloseCompletion, Some("ProviderDialog")),
        KeyBinding::new(
            "enter",
            AcceptCompletion,
            Some("ApiKeyInput > EditableText"),
        ),
    ]);
    for shortcut in shortcuts() {
        let Some(keys) = shortcut_keys(shortcut.action, cx) else {
            continue;
        };
        cx.bind_keys([(shortcut.bind)(&keys, shortcut.context)]);
    }
    set_menus(cx);
}

fn set_menus(cx: &mut App) {
    let app = zp_core::APP_NAME;
    cx.set_menus(vec![
        Menu {
            name: app.into(),
            items: vec![
                MenuItem::action(format!("About {app}"), About),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action(format!("Quit {app}"), Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Chat", NewChat),
                MenuItem::action("New Window", NewWindow),
                MenuItem::action("Open Folder…", OpenFolder),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
                MenuItem::action("Close Window", CloseWindow),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette…", CommandPalette),
                MenuItem::separator(),
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PreviousTab),
                MenuItem::action("Session Tree", ToggleSessionTree),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Changes Panel", ToggleRightPanel),
                MenuItem::action("Toggle Diff Layout", ToggleDiffLayout),
            ],
            disabled: false,
        },
    ]);
}

fn show_about(_: &About, cx: &mut App) {
    let Some(window) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        return;
    };
    let detail = format!("Version {}\nA native GUI agent harness.", zp_core::VERSION);
    let result = window.update(cx, |_, window, cx| {
        // The prompt is informational; its answer is not needed.
        let _answer = window.prompt(
            PromptLevel::Info,
            zp_core::APP_NAME,
            Some(&detail),
            &["OK"],
            cx,
        );
    });
    if let Err(error) = result {
        tracing::warn!(?error, "failed to show the About dialog");
    }
}
