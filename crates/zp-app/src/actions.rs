//! Application-wide actions, key bindings and menus.
//!
//! Key bindings use `secondary`, which is cmd on macOS and ctrl on Linux / Windows.

use gpui::{App, KeyBinding, Menu, MenuItem, PromptLevel, actions};

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
        ToggleRightPanel
    ]
);

/// Key context of project windows.
pub const PROJECT_CONTEXT: &str = "ProjectWindow";

pub const KEY_QUIT: &str = "secondary-q";
pub const KEY_OPEN_FOLDER: &str = "secondary-o";
pub const KEY_NEW_WINDOW: &str = "secondary-shift-n";
pub const KEY_CLOSE_TAB: &str = "secondary-w";
pub const KEY_CLOSE_WINDOW: &str = "secondary-shift-w";
pub const KEY_TOGGLE_SIDEBAR: &str = "secondary-b";
pub const KEY_TOGGLE_RIGHT_PANEL: &str = "secondary-alt-b";

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

    cx.bind_keys([
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
