//! ZP desktop application.
//!
//! `zp [DIR]` opens `DIR` as a project; without an argument the project windows of the
//! previous run are restored (or the welcome window is shown).

mod actions;
mod approvals;
mod changes_panel;
mod command_palette;
mod context_menu;
mod conversation;
mod diff_view;
mod git_panel;
mod markdown;
mod mcp;
mod notifications;
mod project;
mod review_model_picker;
mod selectable_text;
mod settings;
mod store;
mod terminal;
mod theme;
mod ui;
mod updates;
mod welcome;
mod workspace;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_advanced;
#[cfg(test)]
mod tests_changes;
#[cfg(test)]
mod tests_conversation;
#[cfg(test)]
mod tests_git;
#[cfg(test)]
mod tests_permissions;
#[cfg(test)]
mod tests_settings;
#[cfg(test)]
mod tests_tools;
#[cfg(feature = "visual-tests")]
mod visual;

use std::path::PathBuf;

use gpui::App;

use crate::store::{AppModels, AppPaths, AppSettings, AppStore};
use crate::theme::Theme;
use crate::workspace::Workspaces;

fn main() {
    zp_core::logging::init();
    tracing::info!(version = zp_core::VERSION, "starting {}", zp_core::APP_NAME);
    let project_arg = std::env::args_os().nth(1).map(PathBuf::from);
    #[cfg(feature = "visual-tests")]
    if project_arg.as_deref() == Some(std::path::Path::new("--visual-tests")) {
        let out = std::env::args_os()
            .nth(2)
            .map_or_else(|| "/tmp/zp-shots".into(), PathBuf::from);
        visual::run(out);
        return;
    }

    gpui_platform::application().run(move |cx: &mut App| {
        gpui_tokio::init(cx);
        let data_dir = zp_core::paths::data_dir()
            .inspect_err(|error| tracing::warn!(%error, "no data directory; state is not saved"))
            .ok();
        init(data_dir, cx);
        // Refresh stale model catalog layers in the background.
        AppModels::sync(cx, Vec::new(), false);
        updates::check_on_startup(cx);
        cx.on_app_quit(|cx| {
            workspace::prepare_quit(cx);
            async {}
        })
        .detach();

        let opened = match &project_arg {
            Some(path) => workspace::open_project(path, None, cx)
                .map(|_| ())
                .or_else(|error| {
                    tracing::error!(?error, "could not open {}", path.display());
                    workspace::open_welcome(cx).map(|_| ())
                }),
            None => workspace::restore(cx),
        };
        if let Err(error) = opened {
            tracing::error!(?error, "failed to open a window");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

/// Installs the globals every window relies on.
pub fn init(data_dir: Option<PathBuf>, cx: &mut App) {
    AppSettings::init(data_dir.as_deref(), cx);
    Theme::init(cx);
    AppPaths::init(data_dir.clone(), cx);
    AppStore::init(data_dir.as_deref().map(zp_core::paths::state_file), cx);
    AppModels::init(cx);
    Workspaces::init(cx);
    actions::init(cx);
    notifications::init(cx);
    updates::init(cx);
}

/// Applies changed settings to the running app: theme, shortcuts, titles and the
/// permissions of open projects.
pub fn apply_settings(cx: &mut App) {
    Theme::apply_settings(cx);
    actions::bind_keys(cx);
    let titles = AppSettings::get(cx).generate_titles.unwrap_or(true);
    if cx.has_global::<AppModels>() {
        cx.global_mut::<AppModels>().generate_titles = titles;
    }
    workspace::apply_permissions(cx);
    cx.refresh_windows();
}
