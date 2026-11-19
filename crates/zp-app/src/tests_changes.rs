//! M7: inline diffs, the changes panel, reverts, checkpoints, restoring files when moving
//! between branches, the diff view and git status — with real tools in a temp project.

use std::path::{Path, PathBuf};
use std::process::Command;

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use serde_json::json;
use zp_ai::{FauxProvider, FauxResponse};
use zp_core::changes::{RevertStatus, Scope};

use crate::actions::CloseDiff;
use crate::changes_panel::{ChangesMode, ChangesPanel, FileKind};
use crate::conversation::ConversationView;
use crate::conversation::transcript::Row;
use crate::diff_view::DiffLayout;
use crate::project::ProjectWindow;
use crate::tests::project_dir;
use crate::tests_conversation::{active, idle, open, send, setup_models, wait_until};

fn edit(path: &str, old: &str, new: &str) -> FauxResponse {
    FauxResponse::tool_call(
        "edit",
        json!({"path": path, "edits": [{"oldText": old, "newText": new}]}),
    )
}

fn write(path: &str, content: &str) -> FauxResponse {
    FauxResponse::tool_call("write", json!({"path": path, "content": content}))
}

fn read(root: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(root.join(name)).ok()
}

fn panel(window: WindowHandle<ProjectWindow>, cx: &mut TestAppContext) -> Entity<ChangesPanel> {
    window
        .update(cx, |view, _, _| view.changes_panel().clone())
        .unwrap()
}

/// `(path, kind, added, removed)` of the session view.
fn session_files(
    panel: &Entity<ChangesPanel>,
    cx: &mut TestAppContext,
) -> Vec<(String, FileKind, usize, usize)> {
    panel.read_with(cx, |panel, _| {
        panel
            .files()
            .iter()
            .map(|f| (f.display.clone(), f.kind, f.added, f.removed))
            .collect()
    })
}

fn group_labels(panel: &Entity<ChangesPanel>, cx: &mut TestAppContext) -> Vec<String> {
    panel.read_with(cx, |panel, _| {
        panel.groups().iter().map(|g| g.label.clone()).collect()
    })
}

/// Tool call ids of the transcript, in order.
fn tool_calls(conversation: &Entity<ConversationView>, cx: &mut TestAppContext) -> Vec<String> {
    conversation.read_with(cx, |view, _| {
        let transcript = view.transcript();
        (0..transcript.len())
            .filter_map(|row| transcript.assistant(row))
            .flat_map(|message| {
                message
                    .tool_calls()
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    })
}

fn with_window<R>(
    window: WindowHandle<ProjectWindow>,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut ProjectWindow, &mut gpui::Window, &mut gpui::Context<ProjectWindow>) -> R,
) -> R {
    window
        .update(cx, |view, window, cx| f(view, window, cx))
        .unwrap()
}

fn click(window: WindowHandle<ProjectWindow>, selector: &'static str, cx: &mut TestAppContext) {
    wait_until(cx, |cx| {
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds(selector)
            .is_some()
    });
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    let bounds = visual.debug_bounds(selector).unwrap();
    visual.simulate_click(bounds.center(), Modifiers::none());
}

#[gpui::test]
fn turn_footer_counts_net_changes_opens_diffs_and_reverts_the_turn(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(edit("a.txt", "two", "TWO"));
    faux.push(edit("a.txt", "TWO", "final"));
    faux.push(write("new.txt", "new file\n"));
    faux.push(FauxResponse::text("Updated both files."));
    faux.push(FauxResponse::text("No further changes."));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    std::fs::write(project.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "update the files", cx);
    idle(&conversation, 5, cx);
    let summary = conversation.read_with(cx, |view, _| {
        assert!(view.turn_changes_for_row(0).is_none());
        assert!(view.turn_changes_for_row(1).is_none());
        view.turn_changes_for_row(4).unwrap()
    });
    assert_eq!(
        (summary.files.len(), summary.added, summary.removed),
        (2, 2, 1)
    );
    assert_eq!((summary.files[0].added, summary.files[0].removed), (1, 1));
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    assert_eq!(
        conversation.read_with(cx, |view, _| view.turn_changes_for_row(4)),
        Some(summary.clone())
    );
    click(window, "turn-changes-open", cx);
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    assert_eq!(
        diff.read_with(cx, |view, _| view.item().unwrap().display.clone()),
        "a.txt"
    );
    click(window, "diff-file-1", cx);
    assert_eq!(
        diff.read_with(cx, |view, _| view.item().unwrap().display.clone()),
        "new.txt"
    );
    click(window, "diff-file-0", cx);
    assert_eq!(
        diff.read_with(cx, |view, _| view.item().unwrap().display.clone()),
        "a.txt"
    );
    cx.dispatch_action(window.into(), CloseDiff);
    let before_revert_leaf = conversation.read_with(cx, |view, _| view.leaf_id());
    click(window, "turn-changes-revert", cx);
    assert!(cx.has_pending_prompt());
    assert_eq!(
        read(&project, "a.txt").as_deref(),
        Some("one\nfinal\nthree\n")
    );
    assert!(read(&project, "new.txt").is_some());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(read(&project, "new.txt").is_some());
    assert_eq!(
        conversation.read_with(cx, |view, _| view.transcript().len()),
        5
    );
    click(window, "turn-changes-revert", cx);
    cx.simulate_prompt_answer("Revert");
    wait_until(cx, |_| read(&project, "new.txt").is_none());
    assert_eq!(
        read(&project, "a.txt").as_deref(),
        Some("one\ntwo\nthree\n")
    );
    assert!(conversation.read_with(cx, |view, _| {
        matches!(view.transcript().rows().last(), Some(Row::Notice { text, .. }) if text.starts_with("Reverted turn 1"))
    }));
    let saved = cx.update(|cx| crate::store::AppStore::state(cx).project(&project));
    assert_eq!(
        saved.tabs[0].leaf,
        conversation.read_with(cx, |view, _| view.leaf_id())
    );
    assert!(saved.tabs[0].leaf_includes_restores);
    cx.update(|cx| {
        crate::store::AppStore::update(cx, |state| {
            let tab = &mut state.project_mut(&project).tabs[0];
            tab.leaf = before_revert_leaf.clone();
            tab.leaf_includes_restores = false;
        });
    });
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    let reverted_summary =
        conversation.read_with(cx, |view, _| view.turn_changes_for_row(4).unwrap());
    assert_eq!(reverted_summary.status, RevertStatus::Reverted);
    assert_eq!(
        (
            reverted_summary.files.len(),
            reverted_summary.added,
            reverted_summary.removed
        ),
        (2, 2, 1)
    );
    wait_until(cx, |cx| {
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("turn-changes-revert-status")
            .is_some()
    });
    assert!(
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("turn-changes-revert")
            .is_none()
    );
    click(window, "turn-changes-open", cx);
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    diff.read_with(cx, |view, _| {
        assert_eq!(view.item().unwrap().status, RevertStatus::Reverted);
        assert!(view.item().unwrap().revert.is_none());
    });
    click(window, "diff-file-1", cx);
    assert!(diff.read_with(cx, |view, _| view.item().unwrap().revert.is_none()));
    cx.dispatch_action(window.into(), CloseDiff);
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| {
            view.revert(Scope::Group(summary.group), None, window, cx)
        });
    });
    assert!(!cx.has_pending_prompt());
    let reverted_leaf = conversation
        .read_with(cx, |view, _| view.leaf_id())
        .unwrap();
    conversation.update(cx, |view, cx| {
        view.restore_leaf(before_revert_leaf.as_deref().unwrap(), false, cx)
    });
    assert_eq!(
        conversation.read_with(cx, |view, _| view.turn_changes_for_row(4).unwrap().status),
        RevertStatus::Active
    );
    conversation.update(cx, |view, cx| view.restore_leaf(&reverted_leaf, false, cx));
    send(window, "anything else?", cx);
    idle(&conversation, 8, cx);
    assert!(conversation.read_with(cx, |view, _| view.turn_changes_for_row(7).is_none()));
    assert_eq!(
        conversation.read_with(cx, |view, _| view.turn_changes_for_row(4)),
        Some(reverted_summary)
    );
}

#[gpui::test]
fn turn_file_list_expands_and_opens_the_selected_diff(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let paths = [
        "a.txt",
        "nested/b.txt",
        "nested/with/a/long/directory/path/for/the/file/list/c.txt",
        "nested/d.txt",
    ];
    for path in paths {
        faux.push(write(path, &format!("{path}\n")));
    }
    faux.push(FauxResponse::text("Created four files."));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    cx.simulate_window_resize(window.into(), gpui::size(gpui::px(700.), gpui::px(760.)));
    let conversation = active(window, cx);
    send(window, "create the files", cx);
    idle(&conversation, 6, cx);
    let summary = conversation.read_with(cx, |view, _| view.turn_changes_for_row(5).unwrap());
    assert_eq!(
        (summary.files.len(), summary.added, summary.removed),
        (4, 4, 0)
    );
    click(window, "turn-changes-file-1", cx);
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    diff.read_with(cx, |view, _| {
        assert_eq!(view.item().unwrap().display, paths[1])
    });
    cx.dispatch_action(window.into(), CloseDiff);
    let bounds = |selector, cx: &mut TestAppContext| {
        VisualTestContext::from_window(window.into(), cx).debug_bounds(selector)
    };
    assert!(bounds("turn-changes-file-2", cx).is_some());
    assert!(bounds("turn-changes-file-3", cx).is_none());
    click(window, "turn-changes-toggle", cx);
    click(window, "turn-changes-file-3", cx);
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    diff.read_with(cx, |view, _| {
        assert_eq!(view.item().unwrap().display, paths[3])
    });
    for (selector, path) in ["diff-file-0", "diff-file-1", "diff-file-2", "diff-file-3"]
        .into_iter()
        .zip(paths)
    {
        click(window, selector, cx);
        diff.read_with(cx, |view, _| {
            assert_eq!(view.item().unwrap().display, path);
            assert_eq!(
                view.item().unwrap().after.as_deref(),
                Some(format!("{path}\n").as_str())
            );
            assert_eq!(view.diff().added, 1);
        });
    }
    cx.dispatch_action(window.into(), CloseDiff);
    click(window, "turn-changes-toggle", cx);
    cx.run_until_parked();
    assert!(bounds("turn-changes-file-3", cx).is_none());
}

#[gpui::test]
fn individual_edit_card_revert_protects_external_changes(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(edit("a.txt", "one", "ONE"));
    faux.push(FauxResponse::text("Updated the file."));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    std::fs::write(project.join("a.txt"), "one\n").unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "update the file", cx);
    idle(&conversation, 3, cx);
    let call = tool_calls(&conversation, cx).remove(0);
    conversation.update(cx, |view, cx| view.toggle_tool(&call, 1, true, cx));
    click(window, "tool-revert", cx);
    assert!(cx.has_pending_prompt());
    assert_eq!(read(&project, "a.txt").as_deref(), Some("ONE\n"));
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(read(&project, "a.txt").as_deref(), Some("ONE\n"));
    assert_eq!(
        conversation.read_with(cx, |view, _| view.transcript().len()),
        3
    );
    std::fs::write(project.join("a.txt"), "outside edit\n").unwrap();
    click(window, "tool-revert", cx);
    cx.simulate_prompt_answer("Revert");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Skip These Files");
    cx.run_until_parked();
    assert_eq!(read(&project, "a.txt").as_deref(), Some("outside edit\n"));
    click(window, "tool-revert", cx);
    cx.simulate_prompt_answer("Revert");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Overwrite");
    cx.run_until_parked();
    assert_eq!(read(&project, "a.txt").as_deref(), Some("one\n"));
    assert!(conversation.read_with(cx, |view, _| {
        matches!(view.transcript().rows().last(), Some(Row::Notice { text, .. }) if text.starts_with("Reverted edit to a.txt"))
    }));
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    assert!(conversation.read_with(cx, |view, _| view.change_log().tool_reverted(&call)));
    wait_until(cx, |cx| {
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("tool-reverted")
            .is_some()
    });
    assert!(
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("tool-revert")
            .is_none()
    );
    let item = conversation.update(cx, |view, cx| {
        let change = zp_core::checkpoint::RecordedChange::from_details(
            view.transcript()
                .tool(&call)
                .unwrap()
                .details
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        view.tool_diff_item(&call, &change, cx).unwrap()
    });
    assert_eq!(item.status, RevertStatus::Reverted);
    assert!(item.revert.is_none());
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| {
            view.revert(Scope::Group(1), None, window, cx)
        });
    });
    cx.simulate_prompt_answer("Revert");
    wait_until(cx, |_| {
        read(&project, "a.txt").as_deref() == Some("outside edit\n")
    });
    assert!(!conversation.read_with(cx, |view, _| view.change_log().tool_reverted(&call)));
}

#[gpui::test]
fn partial_reverts_survive_reopening_and_skipped_files_remain_active(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    for path in ["a.txt", "b.txt", "c.txt"] {
        faux.push(write(path, "created\n"));
    }
    faux.push(FauxResponse::text("Created three files."));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "create", cx);
    idle(&conversation, 5, cx);
    let calls = tool_calls(&conversation, cx);
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| view.revert_tool_call(&calls[0], window, cx));
    });
    cx.simulate_prompt_answer("Revert");
    wait_until(cx, |_| read(&project, "a.txt").is_none());
    std::fs::write(project.join("b.txt"), "outside\n").unwrap();
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| {
            view.revert(Scope::Group(0), None, window, cx)
        });
    });
    cx.simulate_prompt_answer("Revert");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Skip These Files");
    wait_until(cx, |_| read(&project, "c.txt").is_none());
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    conversation.update(cx, |view, cx| {
        assert_eq!(
            view.turn_changes_for_row(4).unwrap().status,
            RevertStatus::Partial
        );
        let log = view.change_log();
        assert!(log.tool_reverted(&calls[0]));
        assert!(!log.tool_reverted(&calls[1]));
        assert!(log.tool_reverted(&calls[2]));
        let items = view.diff_items(Scope::Group(0), cx);
        assert!(items[0].revert.is_none());
        assert!(items[1].revert.is_some());
        assert!(items[2].revert.is_none());
    });
    assert_eq!(read(&project, "b.txt").as_deref(), Some("outside\n"));
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| view.revert_tool_call(&calls[1], window, cx));
    });
    cx.simulate_prompt_answer("Revert");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Overwrite");
    wait_until(cx, |_| read(&project, "b.txt").is_none());
    assert_eq!(
        conversation.read_with(cx, |view, _| view.turn_changes_for_row(4).unwrap().status),
        RevertStatus::Reverted
    );
}

#[gpui::test]
fn individual_diff_revert_preserves_subsequent_edits(cx: &mut TestAppContext) {
    let (_data, _dir, project, window) = two_turns(cx);
    let conversation = active(window, cx);
    let calls = tool_calls(&conversation, cx);
    let item = conversation.update(cx, |view, cx| {
        let state = view.transcript().tool(&calls[0]).unwrap();
        let change =
            zp_core::checkpoint::RecordedChange::from_details(state.details.as_ref().unwrap())
                .unwrap();
        view.tool_diff_item(&calls[0], &change, cx).unwrap()
    });
    assert!(item.revert.is_some());
    with_window(window, cx, |view, window, cx| {
        view.open_diff(vec![item], 0, window, cx)
    });
    click(window, "diff-revert", cx);
    assert!(cx.has_pending_prompt());
    assert!(with_window(window, cx, |view, _, _| view.diff_view()).is_some());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(
        read(&project, "a.txt").as_deref(),
        Some("one\nTWO\nTHREE\n")
    );
    assert!(with_window(window, cx, |view, _, _| view.diff_view()).is_some());
    click(window, "diff-revert", cx);
    cx.simulate_prompt_answer("Revert");
    wait_until(cx, |_| {
        read(&project, "a.txt").as_deref() == Some("one\ntwo\nTHREE\n")
    });
    assert_eq!(read(&project, "c.txt").as_deref(), Some("new file\n"));
    assert_eq!(read(&project, "b.txt").as_deref(), Some("ALPHA\n"));
    assert!(!cx.has_pending_prompt());
    assert!(with_window(window, cx, |view, _, _| view.diff_view()).is_none());
}

/// A project with `a.txt` and `b.txt`, and an agent that runs two turns of edits.
fn two_turns(
    cx: &mut TestAppContext,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
    WindowHandle<ProjectWindow>,
) {
    let faux = FauxProvider::default();
    faux.push(edit("a.txt", "two", "TWO"));
    faux.push(write("c.txt", "new file\n"));
    faux.push(FauxResponse::text("Turn one done."));
    faux.push(edit("a.txt", "three", "THREE"));
    faux.push(edit("b.txt", "alpha", "ALPHA"));
    faux.push(FauxResponse::text("Turn two done."));
    let data = setup_models(cx, &[&faux], false);
    let (dir, project) = project_dir();
    std::fs::write(project.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(project.join("b.txt"), "alpha\n").unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "first", cx);
    idle(&conversation, 4, cx);
    send(window, "second", cx);
    idle(&conversation, 8, cx);
    (data, dir, project, window)
}

#[gpui::test]
fn inline_diffs_panel_and_reverts(cx: &mut TestAppContext) {
    let (_data, _dir, project, window) = two_turns(cx);
    let conversation = active(window, cx);
    assert_eq!(
        read(&project, "a.txt").as_deref(),
        Some("one\nTWO\nTHREE\n")
    );

    // Each write/edit card has its diff.
    let calls = tool_calls(&conversation, cx);
    assert_eq!(calls.len(), 4);
    let (added, removed, rows) = conversation.read_with(cx, |view, _| {
        let state = view.transcript().tool(&calls[0]).unwrap();
        let change =
            zp_core::checkpoint::RecordedChange::from_details(state.details.as_ref().unwrap())
                .unwrap();
        let diff = view.tool_diff(&calls[0], &change);
        let diff = diff.as_ref().as_ref().unwrap();
        (diff.diff.added, diff.diff.removed, diff.rows.len())
    });
    assert_eq!((added, removed), (1, 1));
    assert_eq!(rows, 5, "a header and four lines");

    // The panel follows the active conversation.
    let panel = panel(window, cx);
    assert_eq!(
        session_files(&panel, cx),
        [
            ("a.txt".into(), FileKind::Modified, 2, 2),
            ("c.txt".into(), FileKind::Added, 1, 0),
            ("b.txt".into(), FileKind::Modified, 1, 1),
        ]
    );
    assert_eq!(
        group_labels(&panel, cx),
        ["Turn 2: second", "Turn 1: first"]
    );

    // Revert turn 1 although turn 2 edited a.txt later: the revert is merged.
    window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.revert(Scope::Group(0), None, window, cx)
            })
        })
        .unwrap();
    cx.simulate_prompt_answer("Revert");
    cx.run_until_parked();
    assert_eq!(
        read(&project, "a.txt").as_deref(),
        Some("one\ntwo\nTHREE\n")
    );
    assert_eq!(read(&project, "c.txt"), None);
    let last_row = conversation.read_with(cx, |view, _| view.transcript().rows().last().cloned());
    assert!(
        matches!(&last_row, Some(Row::Notice { text, .. }) if text == "Reverted turn 1 — a.txt, c.txt (deleted)"),
        "{last_row:?}"
    );
    assert_eq!(
        session_files(&panel, cx),
        [
            ("a.txt".into(), FileKind::Modified, 1, 1),
            ("b.txt".into(), FileKind::Modified, 1, 1),
        ]
    );
    assert_eq!(
        group_labels(&panel, cx),
        ["Reverted turn 1", "Turn 2: second", "Turn 1: first"]
    );

    // A file changed outside ZP in a conflicting way: skip it, then overwrite it.
    std::fs::write(project.join("b.txt"), "user edit\n").unwrap();
    let revert_b = |cx: &mut TestAppContext| {
        window
            .update(cx, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.revert(Scope::Session, Some(project.join("b.txt")), window, cx)
                })
            })
            .unwrap();
        cx.simulate_prompt_answer("Revert");
        cx.run_until_parked();
    };
    revert_b(cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Skip These Files");
    cx.run_until_parked();
    assert_eq!(read(&project, "b.txt").as_deref(), Some("user edit\n"));
    assert_eq!(group_labels(&panel, cx).len(), 3, "nothing was restored");
    revert_b(cx);
    cx.simulate_prompt_answer("Overwrite");
    cx.run_until_parked();
    assert_eq!(read(&project, "b.txt").as_deref(), Some("alpha\n"));

    // The checkpoint before turn 2 brings back the files as they were then (c.txt too).
    window
        .update(cx, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.restore_checkpoint(1, window, cx))
        })
        .unwrap();
    cx.simulate_prompt_answer("Revert");
    cx.run_until_parked();
    assert_eq!(
        read(&project, "a.txt").as_deref(),
        Some("one\nTWO\nthree\n")
    );
    assert_eq!(read(&project, "b.txt").as_deref(), Some("alpha\n"));
    assert_eq!(read(&project, "c.txt").as_deref(), Some("new file\n"));
    assert_eq!(
        group_labels(&panel, cx)[0],
        "Restored the checkpoint before turn 2"
    );
    // The model is told about each restore.
    let restores = conversation.read_with(cx, |view, _| {
        view.transcript()
            .rows()
            .iter()
            .filter(|row| matches!(row, Row::Notice { kind, .. } if kind == zp_core::changes::FILE_RESTORE_TYPE))
            .count()
    });
    assert_eq!(restores, 3);
}

#[gpui::test]
fn diff_view_from_the_panel_and_tool_cards(cx: &mut TestAppContext) {
    let (_data, _dir, _project, window) = two_turns(cx);
    let panel = panel(window, cx);
    let project_root = with_window(window, cx, |view, _, _| view.root().to_path_buf());
    panel.update(cx, |panel, cx| {
        panel.open_file(Scope::Session, &project_root.join("c.txt"), cx)
    });
    cx.run_until_parked();
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).expect("a diff is shown");
    let (display, added, unified_rows) = diff.read_with(cx, |diff, _| {
        (
            diff.item().unwrap().display.clone(),
            diff.diff().added,
            diff.row_count(),
        )
    });
    assert_eq!((display.as_str(), added, unified_rows), ("c.txt", 1, 2));

    // Navigation over the session's files, and both layouts.
    diff.update(cx, |diff, cx| diff.step(1, cx));
    let display = diff.read_with(cx, |diff, _| diff.item().unwrap().display.clone());
    assert_eq!(display, "b.txt");
    diff.update(cx, |diff, cx| diff.set_layout(DiffLayout::Split, cx));
    // Header, then the replaced line side by side.
    assert_eq!(diff.read_with(cx, |diff, _| diff.row_count()), 2);

    // Escape closes it; the layout is remembered for the next diff.
    cx.dispatch_action(window.into(), CloseDiff);
    cx.run_until_parked();
    assert!(with_window(window, cx, |view, _, _| view.diff_view()).is_none());

    // "Open diff" on a tool card.
    let conversation = active(window, cx);
    let calls = tool_calls(&conversation, cx);
    let item = conversation.update(cx, |view, cx| {
        let state = view.transcript().tool(&calls[2]).unwrap();
        let change =
            zp_core::checkpoint::RecordedChange::from_details(state.details.as_ref().unwrap())
                .unwrap();
        view.tool_diff_item(&calls[2], &change, cx).unwrap()
    });
    with_window(window, cx, |view, window, cx| {
        view.open_diff(vec![item], 0, window, cx)
    });
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    let (before, after, layout) = diff.read_with(cx, |diff, _| {
        let item = diff.item().unwrap();
        (
            item.before.as_deref().map(str::to_owned),
            item.after.as_deref().map(str::to_owned),
            diff.layout(),
        )
    });
    assert_eq!(before.as_deref(), Some("one\nTWO\nthree\n"));
    assert_eq!(after.as_deref(), Some("one\nTWO\nTHREE\n"));
    assert_eq!(layout, DiffLayout::Split);
    // Switching tabs returns to the conversation.
    with_window(window, cx, |view, window, cx| {
        view.activate_tab(0, window, cx)
    });
    assert!(with_window(window, cx, |view, _, _| view.diff_view()).is_none());
}

#[gpui::test]
fn moving_between_branches_offers_to_restore_files(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(edit("a.txt", "one", "ONE"));
    faux.push(FauxResponse::text("first done"));
    faux.push(edit("a.txt", "two", "TWO"));
    faux.push(FauxResponse::text("second done"));
    faux.push(FauxResponse::text("edited done"));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    std::fs::write(project.join("a.txt"), "one\ntwo\n").unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "u1", cx);
    idle(&conversation, 3, cx);
    send(window, "u2", cx);
    idle(&conversation, 6, cx);
    assert_eq!(read(&project, "a.txt").as_deref(), Some("ONE\nTWO\n"));

    // Edit & Resend the second message: editing changes nothing until it is sent.
    conversation.update(cx, |view, cx| view.set_draft("my draft", cx));
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| view.edit_and_resend(3, window, cx))
    });
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    let (rows, edited) = conversation.read_with(cx, |view, cx| {
        (
            view.transcript().len(),
            view.editing()
                .map(|edit| edit.input.read(cx).as_str().to_owned()),
        )
    });
    assert_eq!((rows, edited.as_deref()), (6, Some("u2")));
    // Cancelling leaves the conversation as it was.
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| view.cancel_edit(window, cx))
    });
    assert!(conversation.read_with(cx, |view, _| view.editing().is_none()));
    assert_eq!(
        conversation.read_with(cx, |view, _| view.transcript().len()),
        6
    );

    // Sending the edit restores the files to that point and continues from there.
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| {
            view.edit_and_resend(3, window, cx);
            let input = view.editing().unwrap().input.clone();
            input.update(cx, |input, cx| input.emplace("u2 again", cx));
            view.submit_edit(window, cx);
        })
    });
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Restore Files");
    cx.run_until_parked();
    idle(&conversation, 5, cx);
    assert_eq!(read(&project, "a.txt").as_deref(), Some("ONE\ntwo\n"));
    let (draft, last_user) = conversation.read_with(cx, |view, cx| {
        let last_user = view
            .transcript()
            .rows()
            .iter()
            .rev()
            .find_map(|row| match row {
                crate::conversation::transcript::Row::User { text, .. } => Some(text.clone()),
                _ => None,
            });
        (view.draft(cx), last_user)
    });
    assert_eq!(draft, "my draft", "the composer's draft is kept");
    assert_eq!(last_user.as_deref(), Some("u2 again"));
    conversation.update(cx, |view, cx| view.set_draft("", cx));

    // Back to the old branch through the tree, keeping the files: no change on disk.
    let old = conversation.read_with(cx, |view, _| {
        view.tree_rows()
            .into_iter()
            .find(|row| row.label == "u2")
            .unwrap()
            .id
    });
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| view.switch_branch(&old, window, cx))
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Keep Files");
    cx.run_until_parked();
    assert_eq!(read(&project, "a.txt").as_deref(), Some("ONE\ntwo\n"));
    assert_eq!(
        conversation.read_with(cx, |view, _| view.transcript().len()),
        6
    );

    // Cancel leaves everything as it is; restoring brings back the old branch's files.
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| view.branch_from(1, window, cx))
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(
        conversation.read_with(cx, |view, _| view.transcript().len()),
        6
    );
    // The disk does not match this branch (files were kept); going to the start of
    // the conversation restores what the session knows.
    with_window(window, cx, |_, window, cx| {
        conversation.update(cx, |view, cx| {
            view.edit_and_resend(0, window, cx);
            view.submit_edit(window, cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Restore Files");
    cx.run_until_parked();
    // The resent first message finds no scripted answer; only the files matter here.
    idle(&conversation, 2, cx);
    // a.txt was "ONE\ntwo\n" on disk while this branch expects "ONE\nTWO\n": the
    // restore merges and produces the original.
    assert_eq!(read(&project, "a.txt").as_deref(), Some("one\ntwo\n"));
}

#[gpui::test]
fn git_working_tree(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    };
    git(&["init", "-q"]);
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    std::fs::write(project.join("main.rs"), "fn main() {\n    run();\n}\n").unwrap();
    std::fs::write(project.join("new.rs"), "fn run() {}\n").unwrap();

    let window = open(cx, &project);
    let panel = panel(window, cx);
    click(window, "status-changes", cx);
    assert!(with_window(window, cx, |view, _, _| view
        .layout()
        .right_panel_visible));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.mode()),
        ChangesMode::Git
    );
    let git = panel.read_with(cx, |panel, _| panel.git().clone());
    wait_until(cx, |cx| git.read_with(cx, |git, _| git.status().is_some()));
    let files: Vec<String> = git.read_with(cx, |git, _| {
        git.status()
            .unwrap()
            .files
            .iter()
            .map(|f| format!("{} {}", f.status().letter(), f.repo_path))
            .collect()
    });
    assert_eq!(files, ["M main.rs", "U new.rs"]);
    for mode in [ChangesMode::Session, ChangesMode::Turns, ChangesMode::Git] {
        panel.update(cx, |panel, cx| panel.set_mode(mode, cx));
        click(window, "status-changes", cx);
        assert!(!with_window(window, cx, |view, _, _| view
            .layout()
            .right_panel_visible));
        click(window, "status-changes", cx);
        assert_eq!(panel.read_with(cx, |panel, _| panel.mode()), mode);
    }
    git.update(cx, |git, cx| git.open_all(cx));
    wait_until(cx, |cx| {
        with_window(window, cx, |view, _, _| view.diff_view()).is_some()
    });
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    let (caption, added, removed) = diff.read_with(cx, |diff, _| {
        (
            diff.item().unwrap().caption.clone(),
            diff.diff().added,
            diff.diff().removed,
        )
    });
    assert_eq!(
        (caption.as_str(), added, removed),
        ("Working tree vs HEAD", 3, 1)
    );
}

#[gpui::test]
fn diff_view_scrolls_sideways_and_copies_selected_lines(cx: &mut TestAppContext) {
    use gpui::{MouseButton, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px};

    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    };
    git(&["init", "-q"]);
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    let long = format!("    let s = \"{}\";", "x".repeat(400));
    let comment = "    // 中文注释";
    std::fs::write(
        project.join("main.rs"),
        format!("fn main() {{\n{long}\n{comment}\n}}\n"),
    )
    .unwrap();

    let window = open(cx, &project);
    let git_panel = panel(window, cx).read_with(cx, |panel, _| panel.git().clone());
    wait_until(cx, |cx| {
        git_panel.read_with(cx, |git, _| git.status().is_some())
    });
    git_panel.update(cx, |git, cx| git.open_all(cx));
    wait_until(cx, |cx| {
        with_window(window, cx, |view, _, _| view.diff_view()).is_some()
    });
    let diff = with_window(window, cx, |view, _, _| view.diff_view()).unwrap();
    cx.run_until_parked();

    // The long line makes the content wider than the list, which scrolls sideways.
    let (offset, width, content) = diff.read_with(cx, |diff, _| diff.horizontal_scroll());
    assert_eq!(offset, px(0.));
    assert!(content > width + px(1000.), "{content:?} vs {width:?}");
    let selection = diff.read_with(cx, |diff, _| diff.selection());
    let start = selection.point_for_text(comment, 7).expect("comment drawn");
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    visual.simulate_event(ScrollWheelEvent {
        position: start,
        delta: ScrollDelta::Pixels(point(px(-300.), px(0.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    visual.run_until_parked();
    let (offset, ..) = diff.read_with(&visual, |diff, _| diff.horizontal_scroll());
    assert_eq!(offset, px(-300.));
    visual.simulate_event(ScrollWheelEvent {
        position: start,
        delta: ScrollDelta::Pixels(point(px(300.), px(0.))),
        modifiers: Modifiers::none(),
        touch_phase: TouchPhase::Moved,
    });
    visual.run_until_parked();

    // Drag across the wide characters, copy.
    let start = selection.point_for_text(comment, 7).unwrap();
    let end = selection.point_for_text(comment, 19).unwrap();
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    visual.simulate_keystrokes("secondary-c");
    let copied = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap())
    };
    assert_eq!(copied(&mut visual), "中文注释");

    // Across lines, then everything: lines joined by newlines, without the hunk header.
    let first = selection.point_for_text("fn main() {", 3).unwrap();
    visual.simulate_mouse_down(first, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    visual.simulate_keystrokes("secondary-c");
    assert_eq!(
        copied(&mut visual),
        format!("main() {{\n{long}\n    // 中文注释")
    );
    visual.simulate_keystrokes("secondary-a secondary-c");
    assert_eq!(
        copied(&mut visual),
        format!("fn main() {{}}\nfn main() {{\n{long}\n{comment}\n}}")
    );

    // Side by side: a selection down one side leaves out the other side's lines.
    diff.update(&mut visual, |diff, cx| {
        diff.set_layout(DiffLayout::Split, cx)
    });
    visual.run_until_parked();
    let top = selection.point_for_text("fn main() {", 0).unwrap();
    let bottom = selection.point_for_text(comment, 19).unwrap();
    visual.simulate_mouse_down(top, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_move(bottom, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(bottom, MouseButton::Left, Modifiers::none());
    visual.simulate_keystrokes("secondary-c");
    assert_eq!(
        copied(&mut visual),
        format!("fn main() {{\n{long}\n{comment}")
    );
}
