//! ZP desktop application.

mod actions;
mod theme;
mod welcome;

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

use crate::theme::Theme;
use crate::welcome::WelcomeView;

fn main() {
    zp_core::logging::init();
    tracing::info!(version = zp_core::VERSION, "starting {}", zp_core::APP_NAME);

    gpui_platform::application().run(|cx: &mut App| {
        gpui_tokio::init(cx);
        Theme::init(cx);
        actions::init(cx);

        if let Err(error) = open_welcome_window(cx) {
            tracing::error!(?error, "failed to open the welcome window");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

fn open_welcome_window(cx: &mut App) -> anyhow::Result<()> {
    let bounds = Bounds::centered(None, size(px(880.), px(600.)), cx);
    let options = WindowOptions::new()
        .window_bounds(Some(WindowBounds::Windowed(bounds)))
        .window_min_size(Some(size(px(640.), px(420.))))
        .titlebar(Some(TitlebarOptions {
            title: Some(zp_core::APP_NAME.into()),
            ..Default::default()
        }))
        .app_id(Some("dev.zp.ZP".to_owned()));

    cx.open_window(options, |window, cx| {
        cx.new(|cx| WelcomeView::new(window, cx))
    })?;
    Ok(())
}
