//! Window management scenarios on GPUI's test platform.

use std::path::{Path, PathBuf};

use gpui::{TestAppContext, WindowBounds, point, px, size};
use tempfile::TempDir;
use zp_core::app_state::{AppState, WindowGeometry, WindowMode};

use crate::actions::OpenFolder;
use crate::store::AppStore;
use crate::workspace::{self, Workspaces};

/// Initializes the app with a temp data directory, optionally seeded with `state`.
fn setup(cx: &mut TestAppContext, state: Option<AppState>) -> TempDir {
    let data = tempfile::tempdir().unwrap();
    if let Some(state) = state {
        state
            .save(&zp_core::paths::state_file(data.path()))
            .unwrap();
    }
    let dir = data.path().to_path_buf();
    cx.update(|cx| crate::init(Some(dir), cx));
    data
}

fn project_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canonical = zp_core::project::normalize_project_path(dir.path()).unwrap();
    (dir, canonical)
}

fn saved_state(cx: &mut TestAppContext, data: &TempDir) -> AppState {
    cx.update(AppStore::flush);
    AppState::load(&zp_core::paths::state_file(data.path()))
}

#[gpui::test]
fn one_window_per_project(cx: &mut TestAppContext) {
    let data = setup(cx, None);
    let (_a_dir, a) = project_dir();
    let (_b_dir, b) = project_dir();

    let first = cx
        .update(|cx| workspace::open_project(&a, None, cx))
        .unwrap();
    // The same directory through another spelling focuses the existing window.
    let again = cx
        .update(|cx| workspace::open_project(&a.join("."), None, cx))
        .unwrap();
    assert_eq!(first.window_id(), again.window_id());
    let other = cx
        .update(|cx| workspace::open_project(&b, None, cx))
        .unwrap();
    assert_ne!(first.window_id(), other.window_id());
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 2);

    let root = first
        .update(cx, |view, _, _| view.root().to_path_buf())
        .unwrap();
    assert_eq!(root, a);
    let state = saved_state(cx, &data);
    assert_eq!(state.open_projects, [a.clone(), b.clone()]);
    let recent: Vec<&Path> = state
        .recent_projects
        .iter()
        .map(|r| r.path.as_path())
        .collect();
    assert_eq!(recent, [b.as_path(), a.as_path()]);

    let error = cx
        .update(|cx| workspace::open_project(Path::new("/definitely/not/here"), None, cx))
        .unwrap_err();
    assert!(format!("{error:#}").starts_with("Cannot open /definitely/not/here"));
}

#[gpui::test]
fn closing_windows_and_restoring(cx: &mut TestAppContext) {
    let data = setup(cx, None);
    let (_a_dir, a) = project_dir();
    let (_b_dir, b) = project_dir();
    let first = cx
        .update(|cx| workspace::open_project(&a, None, cx))
        .unwrap();
    let second = cx
        .update(|cx| workspace::open_project(&b, None, cx))
        .unwrap();
    cx.run_until_parked();

    // Closing one of several windows forgets it…
    first
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| Workspaces::open_projects(cx)),
        std::slice::from_ref(&b)
    );
    assert_eq!(
        saved_state(cx, &data).open_projects,
        std::slice::from_ref(&b)
    );

    // …but the last window is kept, so the next launch restores it.
    second
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_eq!(saved_state(cx, &data).open_projects, [b]);
}

#[gpui::test]
fn restores_previous_windows_or_shows_welcome(cx: &mut TestAppContext) {
    let (_a_dir, a) = project_dir();
    let (_b_dir, b) = project_dir();
    let missing = a.join("deleted");
    let mut state = AppState::default();
    for path in [&a, &missing, &b] {
        state.mark_open(path);
    }
    state.project_mut(&a).layout.sidebar_width = 333.0;
    let data = setup(cx, Some(state));

    cx.update(workspace::restore).unwrap();
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 2);
    assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 0);
    let window = cx.update(|cx| Workspaces::project_window(&a, cx)).unwrap();
    let width = window
        .update(cx, |view, _, _| view.layout().sidebar_width)
        .unwrap();
    assert_eq!(width, 333.0, "layout restored");
    assert_eq!(saved_state(cx, &data).open_projects, [a, b]);
}

#[gpui::test]
fn shows_welcome_without_previous_windows(cx: &mut TestAppContext) {
    let _data = setup(cx, None);
    cx.update(workspace::restore).unwrap();
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 1);
    assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 1);
}

#[gpui::test]
fn open_folder_from_welcome_replaces_it(cx: &mut TestAppContext) {
    let data = setup(cx, None);
    let (_dir, project) = project_dir();
    let welcome = cx.update(workspace::open_welcome).unwrap();
    cx.run_until_parked();

    cx.dispatch_action(welcome.into(), OpenFolder);
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.directories && !options.files);
        Some(vec![project.clone()])
    });
    cx.run_until_parked();

    assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 0);
    assert_eq!(cx.windows().len(), 1);
    assert!(
        cx.update(|cx| Workspaces::project_window(&project, cx))
            .is_some()
    );
    assert_eq!(saved_state(cx, &data).recent_projects[0].path, project);
}

#[gpui::test]
fn keyboard_toggles_panels_and_persists_layout(cx: &mut TestAppContext) {
    let data = setup(cx, None);
    let (_dir, project) = project_dir();
    let window = cx
        .update(|cx| workspace::open_project(&project, None, cx))
        .unwrap();
    cx.run_until_parked();
    let layout = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| view.layout().clone())
            .unwrap()
    };
    assert!(layout(cx).sidebar_visible && !layout(cx).right_panel_visible);

    cx.simulate_keystrokes(window.into(), "secondary-b");
    cx.simulate_keystrokes(window.into(), "secondary-alt-b");
    cx.run_until_parked();
    assert!(!layout(cx).sidebar_visible && layout(cx).right_panel_visible);

    let saved = saved_state(cx, &data).project(&project).layout;
    assert!(!saved.sidebar_visible && saved.right_panel_visible);
}

#[gpui::test]
fn reopens_at_the_remembered_position(cx: &mut TestAppContext) {
    let (_dir, project) = project_dir();
    let mut state = AppState::default();
    state.project_mut(&project).window = Some(WindowGeometry {
        display: None,
        x: 40.0,
        y: 60.0,
        width: 900.0,
        height: 700.0,
        mode: WindowMode::Windowed,
    });
    let _data = setup(cx, Some(state));
    let window = cx
        .update(|cx| workspace::open_project(&project, None, cx))
        .unwrap();
    let bounds = window
        .update(cx, |_, window, _| window.window_bounds())
        .unwrap();
    assert_eq!(
        bounds,
        WindowBounds::Windowed(gpui::Bounds::new(
            point(px(40.0), px(60.0)),
            size(px(900.0), px(700.0))
        ))
    );
}
