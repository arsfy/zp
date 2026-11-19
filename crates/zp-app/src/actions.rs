//! Application-wide actions, key bindings and menus.
//!
//! Key bindings use `secondary`, which is cmd on macOS and ctrl on Linux / Windows.

use std::path::PathBuf;

use gpui::{App, KeyBinding, Menu, MenuItem, actions};
use gpui_elements::editable_text::actions::{DEFAULT_INPUT_CONTEXT, Enter, default_bindings};

use crate::store::AppStore;
use crate::workspace;

actions!(
    zp,
    [
        About,
        Quit,
        OpenFolder,
        NewWindow,
        ClearRecentFolders,
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
        SubmitAnswers,
        CommitChanges,
        ConfirmBranch,
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
        NewTerminal,
        TerminalFindNext,
        TerminalFindPrevious,
        TerminalCloseSearch
    ]
);

/// Activates the tab at an index (`usize::MAX` for the last tab).
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = zp, no_json)]
pub struct ActivateTab(pub usize);

/// Opens a folder from the recent folders menu.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = zp, no_json)]
pub struct OpenRecentFolder(pub PathBuf);

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
    // Workspace windows handle this themselves so they can be reused.
    cx.on_action(|_: &OpenFolder, cx| workspace::prompt_open_folder(None, cx));
    cx.on_action(|_: &NewWindow, cx| {
        workspace::open_welcome_or_report(cx);
        cx.activate(true);
    });
    cx.on_action(|action: &OpenRecentFolder, cx| {
        workspace::open_recent_folder(&action.0, cx);
    });
    cx.on_action(|_: &ClearRecentFolders, cx| {
        AppStore::update(cx, |state| state.recent_projects.clear());
    });
    cx.on_action(|_: &CloseWindow, cx| workspace::close_active_window(cx));
    // Without a project window (welcome, settings), closing "the tab" closes the window.
    cx.on_action(|_: &CloseTab, cx| workspace::close_active_window(cx));
    bind_keys(cx);

    let mut recent = AppStore::state(cx).recent_projects.clone();
    cx.observe_global::<AppStore>(move |cx| {
        let current = &AppStore::state(cx).recent_projects;
        if *current != recent {
            recent = current.clone();
            set_menus(cx);
        }
    })
    .detach();

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
        KeyBinding::new(
            "secondary-c",
            gpui_elements::editable_text::actions::Copy,
            Some("DiffView"),
        ),
        KeyBinding::new(
            "secondary-a",
            gpui_elements::editable_text::actions::SelectAll,
            Some("DiffView"),
        ),
        KeyBinding::new("shift-enter", Enter, Some(COMPOSER_INPUT)),
        KeyBinding::new("enter", SubmitEdit, Some(MESSAGE_EDITOR_INPUT)),
        KeyBinding::new("shift-enter", Enter, Some(MESSAGE_EDITOR_INPUT)),
        KeyBinding::new("escape", CancelEdit, Some(MESSAGE_EDITOR_INPUT)),
        KeyBinding::new("escape", StopRun, Some(COMPOSER_INPUT)),
        KeyBinding::new("enter", PickFirstModel, Some(MODEL_FILTER_INPUT)),
        KeyBinding::new("escape", ClosePicker, Some(MODEL_FILTER_INPUT)),
        KeyBinding::new(
            "enter",
            TerminalFindNext,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "shift-enter",
            TerminalFindPrevious,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "f3",
            TerminalFindNext,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "shift-f3",
            TerminalFindPrevious,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "escape",
            TerminalCloseSearch,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "secondary-g",
            TerminalFindNext,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "secondary-shift-g",
            TerminalFindPrevious,
            Some("TerminalSearch > EditableText"),
        ),
        KeyBinding::new(
            "down",
            CompletionNext,
            Some("ReviewModelPicker > EditableText"),
        ),
        KeyBinding::new(
            "up",
            CompletionPrevious,
            Some("ReviewModelPicker > EditableText"),
        ),
        KeyBinding::new(
            "enter",
            AcceptCompletion,
            Some("ReviewModelPicker > EditableText"),
        ),
        KeyBinding::new(
            "escape",
            CloseCompletion,
            Some("ReviewModelPicker > EditableText"),
        ),
        KeyBinding::new("down", SelectNextSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("up", SelectPreviousSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("enter", OpenSelectedSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("escape", ClearSessionSearch, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("enter", ConfirmRename, Some(RENAME_INPUT)),
        KeyBinding::new("escape", CancelRename, Some(RENAME_INPUT)),
        KeyBinding::new("escape", DismissMenu, Some("ContextMenu")),
        KeyBinding::new("escape", CloseDiff, Some("DiffView")),
        KeyBinding::new("enter", DenyToolCall, Some("ApprovalReason > EditableText")),
        KeyBinding::new(
            "enter",
            SubmitAnswers,
            Some("QuestionAnswer > EditableText"),
        ),
        KeyBinding::new(
            "secondary-enter",
            CommitChanges,
            Some("CommitMessage > EditableText"),
        ),
        KeyBinding::new("enter", ConfirmBranch, Some("BranchPicker > EditableText")),
        KeyBinding::new("escape", ClosePicker, Some("BranchPicker > EditableText")),
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
    cx.set_menus(app_menus(cx));
    #[cfg(target_os = "macos")]
    {
        cx.set_dock_menu(dock_menu());
        #[cfg(not(any(test, feature = "visual-tests")))]
        crate::macos::sync_recent_folders(cx);
    }
}

fn recent_folder_items(cx: &App) -> Vec<MenuItem> {
    AppStore::state(cx)
        .recent_projects
        .iter()
        .filter(|recent| recent.path.is_dir())
        .map(|recent| {
            MenuItem::action(
                zp_core::project::project_name(&recent.path),
                OpenRecentFolder(recent.path.clone()),
            )
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn dock_menu() -> Vec<MenuItem> {
    // AppKit supplies recent folders and the window switcher above these commands.
    vec![MenuItem::action("New Window", NewWindow)]
}

fn app_menus(cx: &App) -> Vec<Menu> {
    let app = zp_core::APP_NAME;
    let mut recent = recent_folder_items(cx);
    if !recent.is_empty() {
        recent.push(MenuItem::separator());
    }
    let has_recent = !AppStore::state(cx).recent_projects.is_empty();
    recent.push(MenuItem::action("Clear Recent Folders", ClearRecentFolders).disabled(!has_recent));
    let mut menus = vec![
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
                MenuItem::submenu(Menu::new("Open Recent").items(recent)),
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
    ];
    if cfg!(target_os = "macos") {
        // GPUI registers a menu named "Window" as NSApplication's windows menu.
        menus.push(Menu::new("Window").items([MenuItem::action("New Window", NewWindow)]));
    }
    menus
}

fn show_about(_: &About, cx: &mut App) {
    crate::settings::SettingsWindow::open_at(crate::settings::Page::About, cx);
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use zp_core::app_state::AppState;

    use super::*;
    use crate::tests::{project_dir, saved_state, setup};
    use crate::workspace::Workspaces;

    fn recent_menu(cx: &App) -> Menu {
        app_menus(cx)
            .into_iter()
            .find(|menu| menu.name == "File")
            .unwrap()
            .items
            .into_iter()
            .find_map(|item| match item {
                MenuItem::Submenu(menu) if menu.name == "Open Recent" => Some(menu),
                _ => None,
            })
            .unwrap()
    }

    fn recent_paths(items: &[MenuItem]) -> Vec<PathBuf> {
        items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => action
                    .as_any()
                    .downcast_ref::<OpenRecentFolder>()
                    .map(|action| action.0.clone()),
                _ => None,
            })
            .collect()
    }

    fn dispatch(item: MenuItem, cx: &mut TestAppContext) {
        let MenuItem::Action { action, .. } = item else {
            panic!("expected a menu action");
        };
        cx.update(|cx| cx.dispatch_action(action.as_ref()));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn dock_new_window_opens_a_welcome_window(cx: &mut TestAppContext) {
        let _data = setup(cx, None);
        let (_dir, project) = project_dir();
        cx.update(|cx| workspace::open_project(&project, None, cx))
            .unwrap();
        cx.run_until_parked();
        let item = dock_menu().pop().unwrap();
        assert!(matches!(&item, MenuItem::Action { name, action, .. }
            if name == "New Window" && action.as_any().is::<NewWindow>()));
        dispatch(item, cx);
        assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 1);
        assert_eq!(cx.update(|cx| Workspaces::open_projects(cx)), [project]);
    }

    #[gpui::test]
    fn recent_folder_menus_open_reuse_and_clear_projects(cx: &mut TestAppContext) {
        let (_a_dir, a) = project_dir();
        let (_b_dir, b) = project_dir();
        let a = a.join("shared folder");
        let b = b.join("shared folder");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let missing = a.join("missing");
        let mut state = AppState::default();
        for path in [&a, &b, &missing] {
            state.touch_recent(path);
        }
        let data = setup(cx, Some(state));
        cx.update(workspace::open_welcome).unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(recent_paths(&dock_menu()).is_empty());
            if cfg!(target_os = "macos") {
                assert!(app_menus(cx).iter().any(|menu| menu.name == "Window"));
            }
            let menu = recent_menu(cx);
            assert_eq!(recent_paths(&menu.items), [b.clone(), a.clone()]);
            let labels: Vec<_> = menu
                .items
                .iter()
                .filter_map(|item| match item {
                    MenuItem::Action { name, action, .. }
                        if action.as_any().is::<OpenRecentFolder>() =>
                    {
                        Some(name.to_string())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(labels, ["shared folder", "shared folder"]);
        });

        let item = cx.update(|cx| recent_menu(cx).items.remove(1));
        dispatch(item, cx);
        assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 0);
        let first = cx.update(|cx| Workspaces::project_window(&a, cx).unwrap());
        let item = cx.update(|cx| recent_menu(cx).items.remove(1));
        dispatch(item, cx);
        assert_eq!(cx.update(|cx| Workspaces::open_projects(cx).len()), 2);
        let item = cx.update(|cx| recent_menu(cx).items.remove(1));
        dispatch(item, cx);
        assert_eq!(
            cx.update(|cx| Workspaces::project_window(&a, cx)),
            Some(first)
        );
        assert_eq!(cx.update(|cx| Workspaces::open_projects(cx).len()), 2);
        assert_eq!(saved_state(cx, &data).recent_projects[0].path, a);
        let welcome = cx.update(|cx| workspace::open_welcome(cx).unwrap());
        welcome
            .update(cx, |_, window, _| window.activate())
            .unwrap();
        cx.run_until_parked();
        let item = cx.update(|cx| recent_menu(cx).items.remove(0));
        dispatch(item, cx);
        assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 0);
        assert_eq!(cx.update(|cx| Workspaces::open_projects(cx).len()), 2);
        cx.update(|cx| {
            AppStore::init(Some(zp_core::paths::state_file(data.path())), cx);
            assert_eq!(recent_paths(&recent_menu(cx).items), [a.clone(), b.clone()]);
        });

        let item = cx.update(|cx| recent_menu(cx).items.pop().unwrap());
        dispatch(item, cx);
        assert!(saved_state(cx, &data).recent_projects.is_empty());
        assert_eq!(cx.update(|cx| Workspaces::open_projects(cx).len()), 2);
        cx.update(|cx| {
            assert_eq!(dock_menu().len(), 1);
            assert!(recent_menu(cx).items[0].is_disabled());
        });
    }

    #[gpui::test]
    fn recent_folder_removed_after_building_the_menu_is_reported(cx: &mut TestAppContext) {
        let _data = setup(cx, None);
        let (_dir, root) = project_dir();
        let project = root.join("removed");
        std::fs::create_dir(&project).unwrap();
        cx.update(|cx| {
            AppStore::update(cx, |state| state.touch_recent(&project));
            workspace::open_welcome(cx).unwrap();
        });
        cx.run_until_parked();
        let item = cx.update(|cx| recent_menu(cx).items.remove(0));
        std::fs::remove_dir(&project).unwrap();
        dispatch(item, cx);
        assert!(cx.update(|cx| AppStore::state(cx).recent_projects.is_empty()));
        assert!(cx.update(|cx| Workspaces::open_projects(cx)).is_empty());
        assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 1);
    }
}
