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
        DismissMenu
    ]
);

/// Activates the tab at an index (`usize::MAX` for the last tab).
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = zp, no_json)]
pub struct ActivateTab(pub usize);

/// Key context of project windows.
pub const PROJECT_CONTEXT: &str = "ProjectWindow";

pub const KEY_QUIT: &str = "secondary-q";
pub const KEY_OPEN_FOLDER: &str = "secondary-o";
pub const KEY_NEW_WINDOW: &str = "secondary-shift-n";
pub const KEY_CLOSE_TAB: &str = "secondary-w";
pub const KEY_CLOSE_WINDOW: &str = "secondary-shift-w";
pub const KEY_TOGGLE_SIDEBAR: &str = "secondary-b";
pub const KEY_TOGGLE_RIGHT_PANEL: &str = "secondary-alt-b";
pub const KEY_NEW_CHAT: &str = "secondary-t";
pub const KEY_SEND: &str = "enter";

/// The composer's text field.
const COMPOSER_INPUT: &str = "Composer > EditableText";
/// The model picker's filter field.
const MODEL_FILTER_INPUT: &str = "ModelPicker > EditableText";
/// The sidebar's session search field.
const SESSION_SEARCH_INPUT: &str = "SessionSearch > EditableText";
/// The rename field.
const RENAME_INPUT: &str = "RenameInput > EditableText";
pub const KEY_SESSION_TREE: &str = "secondary-shift-t";

/// Registers global action handlers, key bindings and the application menu.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| workspace::quit(cx));
    cx.on_action(show_about);
    // Welcome windows handle this themselves so the project can replace them.
    cx.on_action(|_: &OpenFolder, cx| workspace::prompt_open_folder(None, cx));
    cx.on_action(|_: &NewWindow, cx| workspace::open_welcome_or_report(cx));
    cx.on_action(|_: &CloseWindow, cx| workspace::close_active_window(cx));
    // Until conversations have tabs (M6), closing "the tab" closes the window, as editors
    // do when no tabs are left.
    cx.on_action(|_: &CloseTab, cx| workspace::close_active_window(cx));

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
    // Text fields first, so the more specific composer bindings below override them.
    cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
    cx.bind_keys([
        KeyBinding::new(KEY_SEND, SendMessage, Some(COMPOSER_INPUT)),
        KeyBinding::new("shift-enter", Enter, Some(COMPOSER_INPUT)),
        KeyBinding::new("escape", StopRun, Some(COMPOSER_INPUT)),
        KeyBinding::new("enter", PickFirstModel, Some(MODEL_FILTER_INPUT)),
        KeyBinding::new("escape", ClosePicker, Some(MODEL_FILTER_INPUT)),
        KeyBinding::new(KEY_NEW_CHAT, NewChat, Some(PROJECT_CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(PROJECT_CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(PROJECT_CONTEXT)),
        KeyBinding::new(KEY_SESSION_TREE, ToggleSessionTree, Some(PROJECT_CONTEXT)),
        KeyBinding::new("down", SelectNextSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("up", SelectPreviousSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("enter", OpenSelectedSession, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("escape", ClearSessionSearch, Some(SESSION_SEARCH_INPUT)),
        KeyBinding::new("enter", ConfirmRename, Some(RENAME_INPUT)),
        KeyBinding::new("escape", CancelRename, Some(RENAME_INPUT)),
        KeyBinding::new("escape", DismissMenu, Some("ContextMenu")),
        KeyBinding::new(KEY_QUIT, Quit, None),
        KeyBinding::new(KEY_OPEN_FOLDER, OpenFolder, None),
        KeyBinding::new(KEY_NEW_WINDOW, NewWindow, None),
        KeyBinding::new(KEY_CLOSE_TAB, CloseTab, None),
        KeyBinding::new(KEY_CLOSE_WINDOW, CloseWindow, None),
        KeyBinding::new(KEY_TOGGLE_SIDEBAR, ToggleSidebar, Some(PROJECT_CONTEXT)),
        KeyBinding::new(
            KEY_TOGGLE_RIGHT_PANEL,
            ToggleRightPanel,
            Some(PROJECT_CONTEXT),
        ),
    ]);

    let app = zp_core::APP_NAME;
    cx.set_menus(vec![
        Menu {
            name: app.into(),
            items: vec![
                MenuItem::action(format!("About {app}"), About),
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
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PreviousTab),
                MenuItem::action("Session Tree", ToggleSessionTree),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Changes Panel", ToggleRightPanel),
            ],
            disabled: false,
        },
    ]);

    // Quit once the last window is closed, on every platform.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            workspace::quit(cx);
        }
    })
    .detach();
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
