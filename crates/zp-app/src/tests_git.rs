//! The Git view: staging with checkboxes, commits, push, undoing a pushed commit,
//! committing everything, history diffs, branches and discarding — against a real
//! repository with a local remote.

use std::path::Path;
use std::process::Command;

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use zp_ai::FauxProvider;
use zp_core::git::GitStatus;

use crate::git_panel::{GitPanel, GitTab};
use crate::project::ProjectWindow;
use crate::tests::project_dir;
use crate::tests_conversation::{open, setup_models, wait_until};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
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

/// Waits until no operation runs and the reloaded status satisfies `done`.
fn settle(
    panel: &Entity<GitPanel>,
    cx: &mut TestAppContext,
    mut done: impl FnMut(&GitStatus) -> bool,
) {
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            !panel.is_busy() && panel.status().is_some_and(&mut done)
        })
    });
}

fn staged(panel: &Entity<GitPanel>, cx: &mut TestAppContext) -> Vec<String> {
    panel.read_with(cx, |panel, _| {
        panel
            .status()
            .unwrap()
            .staged()
            .map(|f| f.repo_path.clone())
            .collect()
    })
}

#[gpui::test]
fn staging_keeps_controls_and_file_rows_in_place(cx: &mut TestAppContext) {
    let _data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_dir, project) = project_dir();
    git(&project, &["init", "-q"]);
    git(&project, &["config", "user.name", "Tester"]);
    git(&project, &["config", "user.email", "t@example.com"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    git(&project, &["config", "core.autocrlf", "false"]);
    for name in ["a.txt", "b.txt"] {
        std::fs::write(project.join(name), "before\n").unwrap();
    }
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "init"]);
    for name in ["a.txt", "b.txt"] {
        std::fs::write(project.join(name), "after\n").unwrap();
    }
    let window = open(cx, &project);
    let panel = window
        .update(cx, |view, _, cx| {
            view.changes_panel().read(cx).git().clone()
        })
        .unwrap();
    click(window, "status-changes", cx);
    settle(&panel, cx, |s| s.unstaged().count() == 2);
    panel.update(cx, |panel, cx| panel.toggle_signoff(cx));
    cx.run_until_parked();
    for (all, staging, expected) in [
        (false, true, 1),
        (false, false, 0),
        (true, true, 2),
        (true, false, 0),
    ] {
        let selectors = [
            "git-commit",
            "git-commit-maximize",
            "git-uncommit",
            "git-show-last-commit",
        ];
        let before: Vec<_> = selectors
            .iter()
            .map(|selector| {
                VisualTestContext::from_window(window.into(), cx)
                    .debug_bounds(selector)
                    .unwrap()
            })
            .collect();
        panel.update(cx, |panel, cx| {
            if all {
                panel.set_all_staged(staging, cx);
            } else {
                panel.set_staged(vec!["a.txt".into()], staging, cx);
            }
            assert!(panel.is_busy());
            // A second request is still blocked while staging is in progress.
            panel.set_staged(vec!["b.txt".into()], staging, cx);
        });
        VisualTestContext::from_window(window.into(), cx)
            .update(|window, cx| window.draw(cx).clear(cx));
        for (selector, before) in selectors.into_iter().zip(before) {
            assert_eq!(
                VisualTestContext::from_window(window.into(), cx).debug_bounds(selector),
                Some(before),
                "{selector} moved or disappeared while staging"
            );
        }
        settle(&panel, cx, |s| s.staged().count() == expected);
    }
}

#[gpui::test]
fn stage_commit_push_uncommit_history_branches_and_discard(cx: &mut TestAppContext) {
    let _data = setup_models(cx, &[&FauxProvider::default()], false);
    let remote_dir = tempfile::tempdir().unwrap();
    let remote = zp_core::project::normalize_project_path(remote_dir.path()).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    let (_dir, project) = project_dir();
    git(&project, &["init", "-q"]);
    for (key, value) in [
        ("user.name", "Tester"),
        ("user.email", "t@example.com"),
        ("commit.gpgsign", "false"),
        // Windows runners convert to CRLF on checkout by default.
        ("core.autocrlf", "false"),
    ] {
        git(&project, &["config", key, value]);
    }
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "init"]);
    git(
        &project,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    std::fs::write(project.join("main.rs"), "fn main() {\n    run();\n}\n").unwrap();
    std::fs::write(project.join("new.rs"), "fn run() {}\n").unwrap();

    let window = open(cx, &project);
    let panel = window
        .update(cx, |view, _, cx| {
            view.changes_panel().read(cx).git().clone()
        })
        .unwrap();
    click(window, "status-changes", cx);
    settle(&panel, cx, |s| s.files.len() == 2);
    panel.read_with(cx, |panel, _| {
        let status = panel.status().unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!(status.unstaged().count(), 2);
        assert_eq!(status.files[0].unstaged_stats, (3, 1));
        assert_eq!(panel.suggested_message(), "Update main.rs");
    });

    // The checkbox stages one file; the suggestion follows what is staged.
    click(window, "git-unstaged-check-0", cx);
    settle(&panel, cx, |s| s.staged().count() == 1);
    assert_eq!(staged(&panel, cx), ["main.rs"]);
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.suggested_message()),
        "Update main.rs"
    );

    // Its row opens the staged diff.
    click(window, "git-staged-file-0", cx);
    wait_until(cx, |cx| {
        window
            .update(cx, |view, _, _| view.diff_view())
            .unwrap()
            .is_some()
    });
    let diff = window
        .update(cx, |view, _, _| view.diff_view())
        .unwrap()
        .unwrap();
    assert_eq!(
        diff.read_with(cx, |diff, _| diff.item().unwrap().caption.clone()),
        "Staged (index vs HEAD)"
    );
    window
        .update(cx, |view, window, cx| view.close_diff(window, cx))
        .unwrap();

    // Commit with a typed message; the new file stays unstaged.
    let message = panel.read_with(cx, |panel, _| panel.message_input());
    message.update(cx, |input, cx| input.emplace("Call run", cx));
    click(window, "git-commit", cx);
    settle(&panel, cx, |s| s.last_commit.as_deref() == Some("Call run"));
    panel.read_with(cx, |panel, cx| {
        let status = panel.status().unwrap();
        assert_eq!(status.ahead, 1);
        assert_eq!(status.files.len(), 1);
        assert!(status.files[0].is_untracked());
        assert_eq!(panel.message_input().read(cx).as_str(), "");
    });

    // Push, then undo the pushed commit after confirming.
    click(window, "git-remote", cx);
    settle(&panel, cx, |s| s.ahead == 0);
    assert_eq!(
        git(&remote, &["log", "-1", "--format=%s", "main"]),
        "Call run\n"
    );
    click(window, "git-uncommit", cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Uncommit");
    settle(&panel, cx, |s| s.last_commit.as_deref() == Some("init"));
    assert_eq!(staged(&panel, cx), ["main.rs"]);
    assert_eq!(
        message.read_with(cx, |input, _| input.as_str().to_owned()),
        "Call run"
    );

    // Unstage everything, then "Commit Tracked" (the new file stays out) with the
    // suggested message, by Cmd/Ctrl-Enter in the message.
    click(window, "git-section-staged-check", cx);
    settle(&panel, cx, |s| s.staged().next().is_none());
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.suggested_message()),
        "Update main.rs"
    );
    message.update(cx, |input, cx| input.emplace("", cx));
    window
        .update(cx, |_, window, cx| {
            let focus = gpui::Focusable::focus_handle(message.read(cx), cx);
            focus.focus(window, cx);
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "secondary-enter");
    settle(&panel, cx, |s| {
        s.last_commit.as_deref() == Some("Update main.rs")
    });
    panel.read_with(cx, |panel, _| {
        let files = &panel.status().unwrap().files;
        assert_eq!(files.len(), 1);
        assert!(files[0].is_untracked());
    });
    panel.update(cx, |panel, cx| panel.commit(cx));
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.notice().cloned()),
        Some(("Nothing to commit".to_owned(), true))
    );

    // Commit options from the dropdown: sign off and skip hooks, then amend.
    click(window, "git-unstaged-check-0", cx);
    settle(&panel, cx, |s| s.staged().count() == 1);
    for option in ["Signoff", "Skip Hooks"] {
        click(window, "git-commit-more", cx);
        let menu = panel
            .read_with(cx, |panel, _| panel.menu())
            .expect("a menu");
        assert_eq!(
            menu.read_with(cx, |menu, _| menu.labels()),
            ["Amend", "Signoff", "Skip Hooks"]
        );
        assert_eq!(
            menu.read_with(cx, |menu, _| menu.is_checked(option)),
            Some(false)
        );
        let toggle = menu.read_with(cx, |menu, _| menu.handler(option));
        window
            .update(cx, |_, window, cx| toggle(window, cx))
            .unwrap();
        // Choosing an item closes the menu.
        menu.update(cx, |_, cx| cx.emit(gpui::DismissEvent));
    }
    let options = panel.read_with(cx, |panel, _| panel.commit_options());
    assert!(options.signoff && options.no_verify && !options.amend);
    message.update(cx, |input, cx| input.emplace("Add run", cx));
    click(window, "git-commit", cx);
    settle(&panel, cx, |s| s.last_commit.as_deref() == Some("Add run"));
    assert!(
        git(&project, &["log", "-1", "--format=%B"])
            .contains("Signed-off-by: Tester <t@example.com>")
    );
    // Amend fills in the last message; the edited one replaces it.
    panel.update(cx, |panel, cx| panel.toggle_amend(cx));
    wait_until(cx, |cx| {
        message.read_with(cx, |input, _| input.as_str().starts_with("Add run\n"))
    });
    message.update(cx, |input, cx| input.emplace("Add run()", cx));
    click(window, "git-commit", cx);
    settle(&panel, cx, |s| {
        s.last_commit.as_deref() == Some("Add run()")
    });
    assert!(!panel.read_with(cx, |panel, _| panel.commit_options().amend));
    assert_eq!(
        git(&project, &["log", "--format=%s"]),
        "Add run()\nUpdate main.rs\ninit\n"
    );

    // History, and the diff of a commit.
    click(window, "git-tab-history", cx);
    assert_eq!(panel.read_with(cx, |panel, _| panel.tab()), GitTab::History);
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel
                .history()
                .is_some_and(|h| h.first().is_some_and(|c| c.summary == "Add run()"))
        })
    });
    click(window, "git-commit-1", cx);
    wait_until(cx, |cx| {
        window
            .update(cx, |view, _, _| view.diff_view())
            .unwrap()
            .is_some()
    });
    let diff = window
        .update(cx, |view, _, _| view.diff_view())
        .unwrap()
        .unwrap();
    let (caption, count) = diff.read_with(cx, |diff, _| {
        (diff.item().unwrap().caption.clone(), diff.items().len())
    });
    assert!(caption.ends_with("Update main.rs"), "{caption}");
    assert_eq!(count, 1);
    window
        .update(cx, |view, window, cx| view.close_diff(window, cx))
        .unwrap();
    panel.update(cx, |panel, cx| panel.set_tab(GitTab::Changes, cx));

    // Branches: create one from the picker's filter, then switch back.
    click(window, "git-branch", cx);
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel
                .picker_branches()
                .is_some_and(|names| names == ["main"])
        })
    });
    cx.simulate_input(window.into(), "feature-x");
    cx.simulate_keystrokes(window.into(), "enter");
    settle(&panel, cx, |s| s.branch.as_deref() == Some("feature-x"));
    assert!(panel.read_with(cx, |panel, _| panel.picker_branches().is_none()));
    // The new branch has no upstream: the remote button publishes it.
    click(window, "git-remote", cx);
    settle(&panel, cx, |s| {
        s.upstream.as_deref() == Some("origin/feature-x")
    });
    click(window, "git-branch", cx);
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel.picker_branches().is_some_and(|b| b.len() == 2)
        })
    });
    cx.simulate_input(window.into(), "mai");
    cx.simulate_keystrokes(window.into(), "enter");
    settle(&panel, cx, |s| s.branch.as_deref() == Some("main"));

    // Discarding: tracked changes go back, new files go away.
    std::fs::write(project.join("new.rs"), "changed\n").unwrap();
    std::fs::write(project.join("junk.txt"), "junk\n").unwrap();
    panel.update(cx, |panel, cx| panel.refresh(cx));
    settle(&panel, cx, |s| s.files.len() == 2);
    click(window, "git-stage-all-more", cx);
    let menu = panel
        .read_with(cx, |panel, _| panel.menu())
        .expect("a menu");
    assert_eq!(
        menu.read_with(cx, |menu, _| menu.labels()),
        ["Stage All", "Unstage All", "Discard All Unstaged…"]
    );
    let discard = menu.read_with(cx, |menu, _| menu.handler("Discard All Unstaged…"));
    window
        .update(cx, |_, window, cx| discard(window, cx))
        .unwrap();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Discard");
    settle(&panel, cx, |s| s.files.is_empty());
    assert_eq!(
        std::fs::read_to_string(project.join("new.rs")).unwrap(),
        "fn run() {}\n"
    );
    assert!(!project.join("junk.txt").exists());
}

#[gpui::test]
fn long_history_draws_only_the_commits_in_view_within_the_panel(cx: &mut TestAppContext) {
    let _data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_dir, project) = project_dir();
    git(&project, &["init", "-q"]);
    git(&project, &["config", "user.name", "Tester"]);
    git(&project, &["config", "user.email", "t@example.com"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    for index in 0..150 {
        // The newest summary is far wider than the panel.
        let message = if index == 149 {
            format!("Commit {index} {}", "with a very long summary ".repeat(20))
        } else {
            format!("Commit {index}")
        };
        git(&project, &["commit", "-q", "--allow-empty", "-m", &message]);
    }
    let window = open(cx, &project);
    let panel = window
        .update(cx, |view, _, cx| {
            view.changes_panel().read(cx).git().clone()
        })
        .unwrap();
    click(window, "status-changes", cx);
    click(window, "git-tab-history", cx);
    wait_until(cx, |cx| {
        panel.read_with(cx, |panel, _| {
            panel.history().is_some_and(|history| history.len() == 150)
        })
    });
    cx.run_until_parked();
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    let row = visual.debug_bounds("git-commit-0").unwrap();
    let viewport = visual.update(|window, _| window.viewport_size());
    assert!(
        row.right() <= viewport.width,
        "a long summary is cut off, not drawn past the panel: {row:?} in {viewport:?}"
    );
    assert!(
        visual.debug_bounds("git-commit-149").is_none(),
        "commits out of view are not built"
    );
}
