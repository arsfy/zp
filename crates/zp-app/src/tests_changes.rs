//! M7: inline diffs, the changes panel, reverts, checkpoints, restoring files when moving
//! between branches, the diff view and git status — with real tools in a temp project.

use std::path::{Path, PathBuf};
use std::process::Command;

use gpui::{Entity, TestAppContext, WindowHandle};
use serde_json::json;
use zp_ai::{FauxProvider, FauxResponse};
use zp_core::changes::Scope;

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
    let item = conversation.read_with(cx, |view, _| {
        let state = view.transcript().tool(&calls[2]).unwrap();
        let change =
            zp_core::checkpoint::RecordedChange::from_details(state.details.as_ref().unwrap())
                .unwrap();
        view.tool_diff_item(&calls[2], &change).unwrap()
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
    panel.update(cx, |panel, cx| panel.set_mode(ChangesMode::Git, cx));
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| panel.git_files().is_some())
    });
    let files: Vec<String> = panel.read_with(cx, |panel, _| {
        panel
            .git_files()
            .unwrap()
            .iter()
            .map(|f| format!("{} {}", f.status.letter(), f.repo_path))
            .collect()
    });
    assert_eq!(files, ["M main.rs", "U new.rs"]);
    let file = panel.read_with(cx, |panel, _| panel.git_files().unwrap()[0].clone());
    panel.update(cx, |panel, cx| panel.open_git_file(&file, cx));
    cx.run_until_parked();
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
