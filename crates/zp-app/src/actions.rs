//! Application-wide actions, key bindings and menus.

use gpui::{App, KeyBinding, Menu, MenuItem, PromptLevel, actions};

actions!(zp, [About, Quit]);

/// Registers global action handlers, key bindings and the application menu.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(show_about);

    // `secondary` is cmd on macOS and ctrl on Linux / Windows.
    cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);

    cx.set_menus(vec![Menu {
        name: zp_core::APP_NAME.into(),
        items: vec![
            MenuItem::action(format!("About {}", zp_core::APP_NAME), About),
            MenuItem::separator(),
            MenuItem::action(format!("Quit {}", zp_core::APP_NAME), Quit),
        ],
        disabled: false,
    }]);

    // Quit once the last window is closed, on every platform.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
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
