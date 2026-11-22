//! Window management: one window per project directory, welcome windows, restoring the
//! previous session's windows, and remembering geometry.
//!
//! Closing a project window forgets it for the next launch, except when it is the last
//! window (which quits the app) or the app is quitting: those windows are reopened.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, PathPromptOptions, PromptLevel,
    TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, point, px, size,
};
use zp_core::app_state::{WindowGeometry, WindowMode};

use crate::project::ProjectWindow;
use crate::store::AppStore;
use crate::ui;
use crate::welcome::WelcomeView;

const APP_ID: &str = "dev.zp.ZP";
const PROJECT_SIZE: (f32, f32) = (1280.0, 820.0);
const PROJECT_MIN_SIZE: (f32, f32) = (720.0, 480.0);
const WELCOME_SIZE: (f32, f32) = (880.0, 600.0);
const WELCOME_MIN_SIZE: (f32, f32) = (640.0, 420.0);
/// Offset between windows opened without a remembered position.
const CASCADE: f32 = 28.0;

/// Open windows, by kind.
#[derive(Default)]
pub struct Workspaces {
    projects: HashMap<PathBuf, WindowHandle<ProjectWindow>>,
    welcomes: Vec<WindowHandle<WelcomeView>>,
    quitting: bool,
}

impl Global for Workspaces {}

impl Workspaces {
    pub fn init(cx: &mut App) {
        cx.set_global(Self::default());
    }

    /// The window of the project at `path`, if open.
    pub fn project_window(path: &Path, cx: &App) -> Option<WindowHandle<ProjectWindow>> {
        cx.global::<Self>().projects.get(path).copied()
    }

    /// Open project directories.
    #[cfg(test)]
    pub fn open_projects(cx: &App) -> Vec<PathBuf> {
        cx.global::<Self>().projects.keys().cloned().collect()
    }

    /// Number of open welcome windows.
    #[cfg(test)]
    pub fn welcome_count(cx: &App) -> usize {
        cx.global::<Self>().welcomes.len()
    }

    fn is_welcome(window: AnyWindowHandle, cx: &App) -> bool {
        cx.global::<Self>()
            .welcomes
            .iter()
            .any(|welcome| welcome.window_id() == window.window_id())
    }

    fn contains_window(window: AnyWindowHandle, cx: &App) -> bool {
        Self::is_welcome(window, cx)
            || cx
                .global::<Self>()
                .projects
                .values()
                .any(|project| project.window_id() == window.window_id())
    }

    fn window_count(&self) -> usize {
        self.projects.len() + self.welcomes.len()
    }

    /// Called when a project window's view is released.
    pub(crate) fn project_closed(path: &Path, cx: &mut App) {
        let this = cx.global_mut::<Self>();
        let closed = this.projects.remove(path);
        let keep_for_restore = this.quitting || this.window_count() == 0;
        if !keep_for_restore {
            AppStore::update(cx, |state| state.mark_closed(path));
        }
        if let Some(window) = closed {
            crate::notifications::forget_window(window.into(), cx);
        }
    }

    /// Called when a welcome window's view is released.
    pub(crate) fn welcome_closed(window: AnyWindowHandle, cx: &mut App) {
        cx.global_mut::<Self>()
            .welcomes
            .retain(|welcome| welcome.window_id() != window.window_id());
    }
}

// ----- geometry --------------------------------------------------------------------------

fn bounds_of(geometry: &WindowGeometry) -> Bounds<gpui::Pixels> {
    Bounds::new(
        point(px(geometry.x), px(geometry.y)),
        size(px(geometry.width), px(geometry.height)),
    )
}

/// Window bounds and display for a remembered geometry, falling back to a cascaded,
/// centered window when there is none or it no longer fits a connected display.
fn placement(
    geometry: Option<&WindowGeometry>,
    default: (f32, f32),
    cascade_index: usize,
    cx: &App,
) -> (WindowBounds, Option<gpui::DisplayId>) {
    if let Some(geometry) = geometry.filter(|g| g.width >= 200.0 && g.height >= 150.0) {
        let display = cx
            .displays()
            .into_iter()
            .find(|display| display.uuid().ok().map(|uuid| uuid.to_string()) == geometry.display)
            .or_else(|| cx.primary_display());
        if let Some(display) = display {
            let bounds = bounds_of(geometry);
            if shows_enough_of(bounds, display.bounds()) {
                let window_bounds = match geometry.mode {
                    WindowMode::Windowed => WindowBounds::Windowed(bounds),
                    WindowMode::Maximized => WindowBounds::Maximized(bounds),
                    WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
                };
                return (window_bounds, Some(display.id()));
            }
        }
    }
    let mut bounds = Bounds::centered(None, size(px(default.0), px(default.1)), cx);
    let offset = px(CASCADE * (cascade_index % 8) as f32);
    bounds.origin = point(bounds.origin.x + offset, bounds.origin.y + offset);
    (WindowBounds::Windowed(bounds), None)
}

/// Whether at least 100×100 of a window at `bounds` lies on a display at `display`.
/// Both come from GPUI in the same coordinates: per display on macOS and X11 (the display's
/// origin is zero there), global on Windows, where a secondary display's origin is not.
fn shows_enough_of(bounds: Bounds<gpui::Pixels>, display: Bounds<gpui::Pixels>) -> bool {
    let visible = bounds.intersect(&display);
    visible.size.width >= px(100.0) && visible.size.height >= px(100.0)
}

/// The current geometry of `window`.
pub fn capture_geometry(window: &Window, cx: &App) -> WindowGeometry {
    let (mode, bounds) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (WindowMode::Windowed, bounds),
        WindowBounds::Maximized(bounds) => (WindowMode::Maximized, bounds),
        WindowBounds::Fullscreen(bounds) => (WindowMode::Fullscreen, bounds),
    };
    WindowGeometry {
        display: window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .map(|uuid| uuid.to_string()),
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        width: bounds.size.width.as_f32(),
        height: bounds.size.height.as_f32(),
        mode,
    }
}

pub(crate) fn window_options(
    title: String,
    geometry: Option<&WindowGeometry>,
    default: (f32, f32),
    min: (f32, f32),
    cx: &App,
) -> WindowOptions {
    let cascade = cx.windows().len();
    let (bounds, display) = placement(geometry, default, cascade, cx);
    let mut options = WindowOptions::new()
        .window_bounds(Some(bounds))
        .window_min_size(Some(size(px(min.0), px(min.1))))
        .titlebar(Some(TitlebarOptions {
            title: Some(title.into()),
            appears_transparent: ui::CUSTOM_TITLE_BAR,
            traffic_light_position: ui::CUSTOM_TITLE_BAR
                .then(|| point(px(ui::TRAFFIC_LIGHTS.0), px(ui::TRAFFIC_LIGHTS.1))),
        }))
        .app_id(Some(APP_ID.to_owned()));
    // With a transparent titlebar the frame equals the content, so saved bounds
    // round-trip exactly; our title row moves the window itself.
    options.app_owns_titlebar_drag = ui::CUSTOM_TITLE_BAR;
    options.display_id = display;
    options
}

// ----- opening and closing ---------------------------------------------------------------

/// Opens the project at `path`, or focuses its window if it is already open. `replace`
/// (a welcome window) is closed once the project window exists.
pub fn open_project(
    path: &Path,
    replace: Option<AnyWindowHandle>,
    cx: &mut App,
) -> Result<WindowHandle<ProjectWindow>> {
    let root = zp_core::project::normalize_project_path(path)
        .with_context(|| format!("Cannot open {}", path.display()))?;
    AppStore::update(cx, |state| state.touch_recent(&root));

    if let Some(existing) = Workspaces::project_window(&root, cx) {
        let replaced = replace.filter(|window| Workspaces::is_welcome(*window, cx));
        cx.defer(move |cx| {
            let _ = existing.update(cx, |_, window, _| window.activate());
            if let Some(replaced) = replaced {
                let _ = replaced.update(cx, |_, window, _| window.remove_window());
            }
        });
        return Ok(existing);
    }

    let geometry = AppStore::state(cx).project(&root).window;
    let options = window_options(
        ProjectWindow::title_for(&root),
        geometry.as_ref(),
        PROJECT_SIZE,
        PROJECT_MIN_SIZE,
        cx,
    );
    let window_root = root.clone();
    let handle = cx.open_window(options, move |window, cx| {
        cx.new(|cx| ProjectWindow::new(window_root, window, cx))
    })?;
    cx.global_mut::<Workspaces>()
        .projects
        .insert(root.clone(), handle);
    AppStore::update(cx, |state| state.mark_open(&root));

    if let Some(replaced) = replace.filter(|w| Workspaces::is_welcome(*w, cx)) {
        cx.defer(move |cx| {
            let _ = replaced.update(cx, |_, window, _| window.remove_window());
        });
    }
    Ok(handle)
}

/// Opens an empty welcome window.
pub fn open_welcome(cx: &mut App) -> Result<WindowHandle<WelcomeView>> {
    let geometry = AppStore::state(cx).welcome_window.clone();
    // Only the first welcome window reuses the remembered position.
    let geometry = geometry.filter(|_| cx.windows().is_empty());
    let options = window_options(
        zp_core::APP_NAME.to_owned(),
        geometry.as_ref(),
        WELCOME_SIZE,
        WELCOME_MIN_SIZE,
        cx,
    );
    let handle = cx.open_window(options, |window, cx| {
        cx.new(|cx| WelcomeView::new(window, cx))
    })?;
    cx.global_mut::<Workspaces>().welcomes.push(handle);
    Ok(handle)
}

pub fn open_welcome_or_report(cx: &mut App) {
    if let Err(error) = open_welcome(cx) {
        tracing::error!(?error, "failed to open a window");
    }
}

/// Opens a recent folder, replacing an active welcome window when possible.
pub fn open_recent_folder(path: &Path, cx: &mut App) {
    let replace = cx.active_window().or_else(|| {
        cx.global::<Workspaces>()
            .welcomes
            .last()
            .map(|window| (*window).into())
    });
    if let Err(error) = open_project(path, replace, cx) {
        AppStore::update(cx, |state| state.remove_recent(path));
        report_error("Could not open folder", &error, cx);
    } else {
        cx.activate(true);
    }
}

/// Reopens the project windows that were open last time (skipping vanished directories),
/// or shows the welcome window.
pub fn restore(cx: &mut App) -> Result<()> {
    let previous = AppStore::state(cx).open_projects.clone();
    for path in previous {
        if !path.is_dir() {
            AppStore::update(cx, |state| state.mark_closed(&path));
            continue;
        }
        if let Err(error) = open_project(&path, None, cx) {
            tracing::warn!(path = %path.display(), ?error, "could not reopen project");
        }
    }
    if cx.global::<Workspaces>().projects.is_empty() {
        open_welcome(cx)?;
    }
    Ok(())
}

/// Shows the folder picker, using the requesting workspace window when available.
pub fn prompt_open_folder(source: Option<AnyWindowHandle>, cx: &mut App) {
    let source = source.or_else(|| {
        cx.active_window()
            .filter(|window| Workspaces::contains_window(*window, cx))
    });
    let answer = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Open".into()),
    });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = answer.await else {
            return;
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        cx.update(|cx| {
            let result = match source {
                Some(source) => open_picked_folder(&path, source, cx),
                None => open_project(&path, None, cx).map(|_| ()),
            };
            if let Err(error) = result {
                report_error("Could not open folder", &error, cx);
            }
        });
    })
    .detach();
}

/// Brings an already open project forward, keeping one window per project.
fn focus_open_project(root: &Path, source: AnyWindowHandle, cx: &mut App) -> bool {
    let Some(existing) = Workspaces::project_window(root, cx) else {
        return false;
    };
    AppStore::update(cx, |state| state.touch_recent(root));
    if existing.window_id() != source.window_id() {
        let close_welcome = Workspaces::is_welcome(source, cx);
        cx.defer(move |cx| {
            let _ = existing.update(cx, |_, window, _| window.activate());
            if close_welcome {
                let _ = source.update(cx, |_, window, _| window.remove_window());
            }
        });
    }
    true
}

fn open_picked_folder(path: &Path, source: AnyWindowHandle, cx: &mut App) -> Result<()> {
    let root = zp_core::project::normalize_project_path(path)
        .with_context(|| format!("Cannot open {}", path.display()))?;
    if focus_open_project(&root, source, cx) {
        return Ok(());
    }
    let _ = source.update(cx, |view, window, cx| {
        let project = window.root::<ProjectWindow>().flatten();
        if project.is_some_and(|project| project.read(cx).has_running_work(cx)) {
            let answer = window.prompt(
                PromptLevel::Warning,
                "Switch folders?",
                Some(
                    "Switching folders stops running agents and closes terminals in this project.",
                ),
                &["Switch Folder", "Cancel"],
                cx,
            );
            let original = view.entity_id();
            cx.spawn(async move |cx| {
                if answer.await == Ok(0) {
                    cx.update(|cx| {
                        let _ = source.update(cx, |view, window, cx| {
                            if view.entity_id() == original
                                && !focus_open_project(&root, source, cx)
                            {
                                replace_project(root, window, cx);
                            }
                        });
                    });
                }
            })
            .detach();
        } else {
            replace_project(root, window, cx);
        }
    });
    Ok(())
}

fn replace_project(root: PathBuf, window: &mut Window, cx: &mut App) {
    let geometry = capture_geometry(window, cx);
    let previous = window
        .root::<ProjectWindow>()
        .flatten()
        .map(|project| project.update(cx, |project, cx| project.prepare_folder_switch(cx)));
    let handle = WindowHandle::new(window.window_handle().window_id());
    let project_root = root.clone();
    window.replace_root(cx, |window, cx| {
        ProjectWindow::new(project_root, window, cx)
    });

    let workspaces = cx.global_mut::<Workspaces>();
    if let Some(previous) = &previous {
        workspaces.projects.remove(previous);
    } else {
        workspaces
            .welcomes
            .retain(|welcome| welcome.window_id() != handle.window_id());
    }
    workspaces.projects.insert(root.clone(), handle);
    AppStore::update(cx, |state| {
        if let Some(previous) = previous {
            state.mark_closed(&previous);
            state.project_mut(&previous).window = Some(geometry.clone());
        }
        state.project_mut(&root).window = Some(geometry);
        state.touch_recent(&root);
        state.mark_open(&root);
    });
}

/// Closes the active window.
pub fn close_active_window(cx: &mut App) {
    if let Some(window) = cx.active_window() {
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        });
    }
}

/// Whether restarting would interrupt work in any project.
pub fn has_update_interruptions(cx: &mut App) -> bool {
    let projects: Vec<_> = cx
        .global::<Workspaces>()
        .projects
        .values()
        .copied()
        .collect();
    projects.into_iter().any(|window| {
        window
            .update(cx, |view, _, cx| view.has_update_interruptions(cx))
            .unwrap_or(false)
    })
}

pub fn save_for_update(cx: &mut App) -> Result<()> {
    let projects: Vec<_> = cx
        .global::<Workspaces>()
        .projects
        .values()
        .copied()
        .collect();
    for window in projects {
        window.update(cx, |view, _, cx| view.save_tabs(cx))?;
    }
    AppStore::try_flush(cx)
}

/// Saves state and quits.
pub fn quit(cx: &mut App) {
    cx.global_mut::<Workspaces>().quitting = true;
    AppStore::flush(cx);
    cx.quit();
}

/// Marks the app as quitting (windows closing now stay remembered) and saves.
pub fn prepare_quit(cx: &mut App) {
    if let Some(workspaces) = cx.try_global::<Workspaces>()
        && !workspaces.quitting
    {
        cx.global_mut::<Workspaces>().quitting = true;
    }
    AppStore::flush(cx);
}

/// Shows `error` in the active window (or logs it without one).
pub fn report_error(title: &str, error: &anyhow::Error, cx: &mut App) {
    tracing::warn!(?error, "{title}");
    let detail = format!("{error:#}");
    let title = title.to_owned();
    let Some(window) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        return;
    };
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| {
            let _answer = window.prompt(PromptLevel::Warning, &title, Some(&detail), &["OK"], cx);
        });
    });
}

/// Re-applies the permission settings to every open project.
pub fn apply_permissions(cx: &mut App) {
    for window in cx.windows() {
        let _ = window.update(cx, |view, _, cx| {
            if let Ok(project) = view.downcast::<ProjectWindow>() {
                project.update(cx, |view, cx| view.apply_permissions(cx));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use super::shows_enough_of;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<gpui::Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    #[test]
    fn windows_are_placed_on_displays_with_a_nonzero_origin() {
        // Windows: a secondary display right of the primary and one left of it, in global
        // coordinates like the windows on them.
        let right = rect(1920.0, 0.0, 2560.0, 1440.0);
        let left = rect(-1920.0, -200.0, 1920.0, 1080.0);
        assert!(shows_enough_of(rect(2200.0, 100.0, 1200.0, 800.0), right));
        assert!(shows_enough_of(rect(-1500.0, 0.0, 1200.0, 800.0), left));
        // Mostly off the display.
        assert!(!shows_enough_of(rect(4450.0, 100.0, 1200.0, 800.0), right));
        assert!(!shows_enough_of(rect(100.0, 100.0, 1200.0, 800.0), right));
        // macOS and X11: displays and windows are relative to the display.
        let display = rect(0.0, 0.0, 1512.0, 982.0);
        assert!(shows_enough_of(rect(40.0, 40.0, 1200.0, 800.0), display));
        assert!(!shows_enough_of(rect(1450.0, 40.0, 1200.0, 800.0), display));
    }
}
