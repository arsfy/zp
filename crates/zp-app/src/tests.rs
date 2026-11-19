//! Window management scenarios on GPUI's test platform.

use std::path::{Path, PathBuf};

use gpui::{TestAppContext, WindowBounds, point, px, size};
use tempfile::TempDir;
use zp_core::app_state::{AppState, WindowGeometry, WindowMode};

use crate::actions::OpenFolder;
use crate::store::AppStore;
use crate::workspace::{self, Workspaces};

/// Initializes the app with a temp data directory, optionally seeded with `state`.
pub(crate) fn setup(cx: &mut TestAppContext, state: Option<AppState>) -> TempDir {
    let data = tempfile::tempdir().unwrap();
    if let Some(state) = state {
        state
            .save(&zp_core::paths::state_file(data.path()))
            .unwrap();
    }
    let dir = data.path().to_path_buf();
    cx.update(|cx| {
        crate::init(Some(dir), cx);
        // Blocks open and close at once; animation tests turn motion back on.
        cx.set_reduce_motion(true);
    });
    data
}

pub(crate) fn project_dir() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canonical = zp_core::project::normalize_project_path(dir.path()).unwrap();
    (dir, canonical)
}

pub(crate) fn saved_state(cx: &mut TestAppContext, data: &TempDir) -> AppState {
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
    let opened = cx.update(|cx| Workspaces::project_window(&project, cx).unwrap());
    assert_eq!(opened.window_id(), welcome.window_id());
    assert_eq!(saved_state(cx, &data).recent_projects[0].path, project);
}

#[gpui::test]
fn open_folder_switches_the_current_project_and_recovers_its_draft(cx: &mut TestAppContext) {
    let data = setup(cx, None);
    let (_a_dir, a) = project_dir();
    let (_b_dir, b) = project_dir();
    let window = cx
        .update(|cx| workspace::open_project(&a, None, cx))
        .unwrap();
    let original = window.root(cx).unwrap();
    cx.run_until_parked();
    let bounds = window
        .update(cx, |_, window, _| window.window_bounds())
        .unwrap();
    let project_a = crate::tests_conversation::active(window, cx);
    project_a.update(cx, |view, cx| view.set_draft("saved for project A", cx));

    let pick = |path: &Path, cx: &mut TestAppContext| {
        cx.dispatch_action(window.into(), OpenFolder);
        cx.run_until_parked();
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|_| Some(vec![path.to_owned()]));
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(
            cx.update(|cx| Workspaces::project_window(path, cx)),
            Some(window)
        );
        window
            .update(cx, |view, window, _| {
                assert_eq!(view.root(), path);
                assert_eq!(window.window_bounds(), bounds);
            })
            .unwrap();
    };

    pick(&b, cx);
    assert!(!cx.has_pending_prompt());
    assert_eq!(saved_state(cx, &data).open_projects, [b.clone()]);
    assert!(cx.update(|cx| crate::mcp::hub(&a, cx)).is_none());
    assert!(
        crate::tests_conversation::active(window, cx)
            .read_with(cx, |view, cx| view.draft(cx).is_empty())
    );

    pick(&a, cx);
    drop(original);
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| Workspaces::project_window(&a, cx)),
        Some(window)
    );
    assert!(cx.update(|cx| crate::mcp::hub(&a, cx)).is_some());
    assert_eq!(saved_state(cx, &data).open_projects, [a]);
    assert_eq!(
        crate::tests_conversation::active(window, cx).read_with(cx, |view, cx| view.draft(cx)),
        "saved for project A"
    );
}

#[gpui::test]
fn open_folder_asks_before_interrupting_a_run(cx: &mut TestAppContext) {
    use std::time::Duration;

    use zp_ai::{FauxProvider, FauxResponse};

    use crate::tests_conversation::{active, open, send, setup_models};

    let faux = FauxProvider::default().with_chunk_delay(Duration::from_secs(30));
    faux.push(FauxResponse::text("Still working"));
    let _data = setup_models(cx, &[&faux], false);
    let (_a_dir, a) = project_dir();
    let (_b_dir, b) = project_dir();
    let window = open(cx, &a);
    send(window, "start a run", cx);
    let conversation = active(window, cx);
    assert!(conversation.read_with(cx, |view, _| view.is_running()));

    let pick_b = |cx: &mut TestAppContext| {
        cx.dispatch_action(window.into(), OpenFolder);
        cx.run_until_parked();
        cx.simulate_path_prompt_response(|_| Some(vec![b.clone()]));
        cx.run_until_parked();
        assert_eq!(cx.pending_prompt().unwrap().0, "Switch folders?");
    };
    pick_b(cx);
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(conversation.read_with(cx, |view, _| view.is_running()));
    assert_eq!(
        window
            .update(cx, |view, _, _| view.root().to_owned())
            .unwrap(),
        a
    );

    pick_b(cx);
    cx.simulate_prompt_answer("Switch Folder");
    cx.run_until_parked();
    assert!(!conversation.read_with(cx, |view, _| view.is_running()));
    assert_eq!(cx.windows().len(), 1);
    assert_eq!(
        window
            .update(cx, |view, _, _| view.root().to_owned())
            .unwrap(),
        b
    );
}

#[gpui::test]
fn open_folder_focuses_a_project_that_is_already_open(cx: &mut TestAppContext) {
    let _data = setup(cx, None);
    let (_a_dir, a) = project_dir();
    let (_b_dir, b) = project_dir();
    let first = cx
        .update(|cx| workspace::open_project(&a, None, cx))
        .unwrap();
    let second = cx
        .update(|cx| workspace::open_project(&b, None, cx))
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(first.into(), OpenFolder);
    cx.run_until_parked();
    cx.simulate_path_prompt_response(|_| Some(vec![b.clone()]));
    cx.run_until_parked();

    assert_eq!(cx.windows().len(), 2);
    assert_eq!(
        cx.update(|cx| Workspaces::project_window(&a, cx)),
        Some(first)
    );
    assert_eq!(
        cx.update(|cx| Workspaces::project_window(&b, cx)),
        Some(second)
    );
    assert_eq!(
        cx.update(|cx| cx.active_window().unwrap().window_id()),
        second.window_id()
    );
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

#[gpui::test]
fn panels_resize_by_dragging_their_border(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, VisualTestContext};
    let _data = setup(cx, None);
    let (_dir, project) = project_dir();
    let window = cx
        .update(|cx| workspace::open_project(&project, None, cx))
        .unwrap();
    cx.run_until_parked();
    cx.dispatch_action(window.into(), crate::actions::ToggleRightPanel);
    cx.run_until_parked();
    let (sidebar, right, width) = window
        .update(cx, |view, window, _| {
            let layout = view.layout();
            (
                layout.sidebar_width,
                layout.right_panel_width,
                window.viewport_size().width.as_f32(),
            )
        })
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let none = Modifiers::none();
    let drag = |from: f32, to: f32, cx: &mut VisualTestContext| {
        let y = 300.0;
        cx.simulate_mouse_down(point(px(from), px(y)), MouseButton::Left, none);
        for step in 1..=4 {
            let x = from + (to - from) * step as f32 / 4.0;
            cx.simulate_mouse_move(point(px(x), px(y)), MouseButton::Left, none);
        }
        cx.simulate_mouse_up(point(px(to), px(y)), MouseButton::Left, none);
        cx.run_until_parked();
    };

    // Pressing right on the sidebar's border line drags it.
    drag(sidebar - 0.5, 320.0, &mut cx);
    let layout = window
        .update(&mut cx, |view, _, _| view.layout().clone())
        .unwrap();
    assert_eq!(layout.sidebar_width, 320.0);

    // The right panel's border, from just outside it (the center side).
    drag(width - right - 2.0, width - 400.0, &mut cx);
    let layout = window
        .update(&mut cx, |view, _, _| view.layout().clone())
        .unwrap();
    assert_eq!(layout.right_panel_width, 400.0);
    assert_eq!(layout.sidebar_width, 320.0);

    // Away from the borders nothing resizes.
    drag(layout.sidebar_width + 80.0, 500.0, &mut cx);
    let layout = window
        .update(&mut cx, |view, _, _| view.layout().clone())
        .unwrap();
    assert_eq!(layout.sidebar_width, 320.0);
}
