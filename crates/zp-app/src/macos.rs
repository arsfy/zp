//! Native recent folders shown above the Dock's window list and application commands.

use gpui::App;

/// Updates the system list from the app's persisted history, including removals.
#[cfg(not(any(test, feature = "visual-tests")))]
pub fn sync_recent_folders(cx: &App) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSDocumentController;

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let controller = NSDocumentController::sharedDocumentController(main_thread);
    // AppKit requires the main thread; a nil sender is valid for this action.
    #[allow(unsafe_code)]
    unsafe {
        controller.clearRecentDocuments(None);
    }
    for recent in crate::store::AppStore::state(cx)
        .recent_projects
        .iter()
        .rev()
    {
        if recent.path.is_dir() {
            cx.add_recent_document(&recent.path);
        }
    }
}

/// Handles file URLs delivered by macOS when a Dock recent folder is selected.
pub fn open_folders(urls: Vec<String>, cx: &mut App) {
    for raw in urls {
        let Some(path) = url::Url::parse(&raw)
            .ok()
            .and_then(|url| url.to_file_path().ok())
        else {
            continue;
        };
        crate::workspace::open_recent_folder(&path, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;
    use crate::tests::{project_dir, setup};
    use crate::workspace::{self, Workspaces};

    #[gpui::test]
    fn dock_file_urls_open_and_reuse_folders_with_spaces_and_unicode(cx: &mut TestAppContext) {
        let _data = setup(cx, None);
        let (_dir, root) = project_dir();
        let project = root.join("shared folder \u{9879}\u{76ee}");
        std::fs::create_dir(&project).unwrap();
        let url = url::Url::from_directory_path(&project).unwrap().to_string();
        cx.update(workspace::open_welcome).unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            open_folders(
                vec![
                    "https://example.invalid/folder".into(),
                    "invalid URL".into(),
                ],
                cx,
            );
        });
        cx.run_until_parked();
        assert!(cx.update(|cx| Workspaces::open_projects(cx)).is_empty());
        assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 1);
        cx.update(|cx| open_folders(vec![url.clone()], cx));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| Workspaces::welcome_count(cx)), 0);
        let first = cx.update(|cx| Workspaces::project_window(&project, cx).unwrap());
        cx.update(|cx| open_folders(vec![url], cx));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| Workspaces::project_window(&project, cx)),
            Some(first)
        );
        assert_eq!(cx.update(|cx| Workspaces::open_projects(cx)), [project]);
    }
}
