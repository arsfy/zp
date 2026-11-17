//! Screenshots of the real UI, rendered off-screen with the platform renderer, driven
//! by a scripted provider. A developer tool, not part of normal builds:
//!
//! ```text
//! cargo run -p zp-app --features visual-tests -- --visual-tests /tmp/zp-shots
//! ```
//!
//! AppKit must run on the main thread, which is why this is a mode of the binary rather
//! than a `#[test]`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Modifiers, MouseButton, VisualTestAppContext, point, px, size};
use serde_json::json;
use zp_agent::{AgentEvent, AgentMessage, AgentToolResult};
use zp_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, FauxBlock, FauxProvider,
    FauxResponse, Models, StopReason, TextContent, ThinkingContent, ToolCall,
};

use crate::actions::SendMessage;
use crate::conversation::Picker;
use crate::project::ProjectWindow;
use crate::store::AppModels;

const FINAL_ANSWER: &str = "I fixed the bug in `add`: it subtracted instead of adding.

## What changed

1. Replaced `a - b` with `a + b` in **lib.rs**
2. Ran the tests — they pass now

```rust
fn add(a: i32, b: i32) -> i32 {
    a + b
}
```

| File | Change |
|------|--------|
| lib.rs | `-` → `+` |

See the [Rust book](https://doc.rust-lang.org/book/) for details. ~~Nothing else~~ changed.";

fn pump(cx: &mut VisualTestAppContext, real: Duration) {
    let rounds = (real.as_millis() / 20).max(1);
    for _ in 0..rounds {
        std::thread::sleep(Duration::from_millis(20));
        cx.advance_clock(Duration::from_millis(40));
        cx.run_until_parked();
    }
}

/// Renders the conversation scenarios into `out`.
pub fn run(out: PathBuf) {
    std::fs::create_dir_all(&out).unwrap();
    let data = tempfile::tempdir().unwrap();
    // The scripted runs need no approvals (the approvals scene asks for them).
    let mut settings = zp_core::settings::Settings::default();
    settings.permissions.mode = Some(zp_core::permissions::PermissionMode::FullAuto);
    settings
        .save(&zp_core::paths::settings_file(data.path()))
        .unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("lib.rs"),
        "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join("test.sh"),
        "echo running tests\nif grep -q 'a + b' lib.rs; then echo 'test add ... ok'; else echo 'test add ... FAILED'; exit 1; fi\n",
    )
    .unwrap();
    let root = zp_core::project::normalize_project_path(project.path()).unwrap();

    let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(150));
    faux.push(FauxResponse::blocks(vec![
        FauxBlock::Thinking(
            "The user says add() is broken. I should read lib.rs first, then run the tests.".into(),
        ),
        FauxBlock::Text("Let me look at the code first.".into()),
        FauxBlock::ToolCall {
            name: "read".into(),
            arguments: json!({"path": "lib.rs"}),
        },
    ]));
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "sh test.sh"}),
    ));
    faux.push(FauxResponse::tool_call(
        "edit",
        json!({"path": "lib.rs", "edits": [{"oldText": "a - b", "newText": "a + b"}]}),
    ));
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "sh test.sh"}),
    ));
    faux.push(FauxResponse::text(FINAL_ANSWER));

    let mut cx = VisualTestAppContext::new(gpui_platform::current_platform(false));
    let data_dir = data.path().to_path_buf();
    cx.update(|cx| {
        crate::init(Some(data_dir), cx);
        gpui_tokio::init(cx);
        let mut models = Models::new();
        models.set_provider(Arc::new(faux.clone()));
        AppModels::set(models, false, cx);
    });
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("transcript-display") {
        transcript_display_settings(&mut cx, &out);
        println!(
            "transcript display screenshots written to {}",
            out.display()
        );
        return;
    }
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("git") {
        git_panel(&mut cx, &out);
        println!("git screenshots written to {}", out.display());
        return;
    }
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("turn-changes") {
        turn_change_footers(&mut cx, &out);
        println!("turn change screenshots written to {}", out.display());
        return;
    }
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("terminal") {
        terminal_interactions(&mut cx, &root, &out);
        println!("terminal screenshots written to {}", out.display());
        return;
    }
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("review-model") {
        review_model_dialogs(&mut cx, &root, &out);
        println!("review model screenshots written to {}", out.display());
        return;
    }
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("live-status") {
        for (name, width, height) in [("desktop", 1280., 820.), ("compact", 800., 620.)] {
            let window = cx
                .open_offscreen_window(size(px(width), px(height)), |window, cx| {
                    let session = zp_session::Session::in_memory(&root);
                    cx.new(|cx| ProjectWindow::new_with_session(root.clone(), session, window, cx))
                })
                .unwrap();
            cx.update_window(window.into(), |view, _, cx| {
                let conversation = view.downcast::<ProjectWindow>().unwrap();
                let conversation = conversation.read(cx).active_conversation().unwrap().clone();
                conversation.update(cx, |view, cx| {
                    let mut events = streaming_text_events();
                    events.push(AgentEvent::MessageUpdate {
                        event: AssistantMessageEvent::Usage {
                            usage: zp_ai::Usage {
                                input: 3500,
                                cache_read: 6500,
                                total_tokens: 10000,
                                ..Default::default()
                            },
                        },
                    });
                    view.inject(events, true, cx);
                });
            })
            .unwrap();
            pump(&mut cx, Duration::from_millis(1200));
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(out.join(format!("live-status-{name}.png")))
                .unwrap();
        }
        println!("live status screenshots written to {}", out.display());
        return;
    }
    inline_code_layout(&mut cx, &root, &out);
    if std::env::var("ZP_VISUAL_SCENE").as_deref() == Ok("inline-code") {
        println!("inline code screenshots written to {}", out.display());
        return;
    }
    let window = cx
        .open_offscreen_window(size(px(1280.), px(820.)), |window, cx| {
            cx.new(|cx| ProjectWindow::new(root.clone(), window, cx))
        })
        .unwrap();
    pump(&mut cx, Duration::from_millis(200));
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(out.join("1-empty.png"))
        .unwrap();

    cx.simulate_input(
        window.into(),
        "add() returns the wrong result, please fix it",
    );
    pump(&mut cx, Duration::from_millis(100));
    cx.dispatch_action(window.into(), SendMessage);
    pump(&mut cx, Duration::from_millis(2600));
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(out.join("2-streaming.png"))
        .unwrap();

    pump(&mut cx, Duration::from_millis(8000));
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(out.join("3-done.png"))
        .unwrap();

    let conversation = cx.update(|cx| {
        window
            .read(cx)
            .map(|view| view.active_conversation().unwrap().clone())
            .unwrap()
    });
    let selection = cx.update(|cx| conversation.read(cx).text_selection());
    let code = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";
    let from = selection
        .point_for_text(code, 0)
        .expect("visible fenced code");
    let to = selection.point_for_text(code, code.len()).unwrap();
    cx.simulate_mouse_down(window.into(), from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(window.into(), to, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(window.into(), to, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes(window.into(), "secondary-c");
    assert_eq!(
        cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()),
        code.trim_end()
    );
    pump(&mut cx, Duration::from_millis(100));
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(out.join("3-code-selected.png"))
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        conversation.update(cx, |view, cx| view.toggle_picker(Picker::Model, window, cx));
    })
    .unwrap();
    pump(&mut cx, Duration::from_millis(100));
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(out.join("4-model-picker.png"))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project.path().join("lib.rs")).unwrap(),
        "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"
    );
    // Mid-run states, from scripted events (the run above finishes between frames).
    for (name, events) in [
        ("5-tool-running.png", tool_running_events()),
        ("6-streaming-text.png", streaming_text_events()),
        ("7-error.png", error_events()),
    ] {
        let root = root.clone();
        let window = cx
            .open_offscreen_window(size(px(1280.), px(820.)), |window, cx| {
                let session = zp_session::Session::in_memory(&root);
                cx.new(|cx| ProjectWindow::new_with_session(root, session, window, cx))
            })
            .unwrap();
        let conversation = cx.update(|cx| {
            window
                .read(cx)
                .map(|view| view.active_conversation().unwrap().clone())
                .unwrap()
        });
        let running = !name.contains("error");
        cx.update(|cx| conversation.update(cx, |view, cx| view.inject(events, running, cx)));
        pump(&mut cx, Duration::from_millis(100));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(name))
            .unwrap();
    }
    // The terminal panel, with some colored output.
    cx.update(|cx| {
        window
            .update(cx, |view, window, cx| view.toggle_terminal(window, cx))
            .unwrap()
    });
    pump(&mut cx, Duration::from_millis(300));
    let terminal = cx.update(|cx| window.read(cx).unwrap().terminal_for_screenshot());
    if let Some(terminal) = terminal {
        cx.update(|cx| {
            terminal.update(cx, |view, _| {
                view.write(
                    b"clear; printf '\\033[32mok\\033[0m tests passed  \\033[1;31merror\\033[0m  \\033[44m blue \\033[0m\\n'; ls\r"
                        .to_vec(),
                )
            })
        });
        pump(&mut cx, Duration::from_millis(1500));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join("31-terminal.png"))
            .unwrap();
        // A second terminal shows the list of terminals.
        cx.update(|cx| {
            window
                .update(cx, |view, window, cx| view.new_terminal(window, cx))
                .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |view, window, cx| {
                    let tabs = view.terminal_tabs_for_screenshot();
                    view.rename_terminal(tabs[0], Some("dev server".into()), window, cx);
                })
                .unwrap()
        });
        if let Some(second) = cx.update(|cx| window.read(cx).unwrap().terminal_for_screenshot()) {
            cx.update(|cx| second.update(cx, |view, _| view.write(b"sleep 30\r".to_vec())));
        }
        pump(&mut cx, Duration::from_millis(1500));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join("32-terminal-tabs.png"))
            .unwrap();
    }
    tabs_and_history(&mut cx, &data_dir_of(&data), &root, &out);
    changes(&mut cx, &faux, &out);
    approvals(&mut cx, &faux, &out);
    settings_pages(&mut cx, &out);
    advanced(&mut cx, &faux, &data_dir_of(&data), &out);
    println!("screenshots written to {}", out.display());
}

fn data_dir_of(data: &tempfile::TempDir) -> PathBuf {
    data.path().to_path_buf()
}

fn turn_change_footers(cx: &mut VisualTestAppContext, out: &std::path::Path) {
    cx.update(|cx| cx.set_prompt_builder(gpui::fallback_prompt_renderer));
    for (width, suffix) in [(1100., ""), (700., "-narrow")] {
        let project = tempfile::tempdir().unwrap();
        let root = zp_core::project::normalize_project_path(project.path()).unwrap();
        let original = "pub fn sum(a: i32, b: i32) -> i32 {\n    a - b\n}\n";
        std::fs::write(root.join("math.rs"), original).unwrap();
        let faux = FauxProvider::default();
        faux.push(FauxResponse::tool_call(
            "edit",
            json!({"path": "math.rs", "edits": [{"oldText": "a - b", "newText": "a + b"}]}),
        ));
        faux.push(FauxResponse::tool_call("write", json!({"path": "tests/sum.rs", "content": "#[test]\nfn sums_numbers() {\n    assert_eq!(sum(2, 3), 5);\n}\n"})));
        faux.push(FauxResponse::tool_call(
            "write",
            json!({"path": "README.md", "content": "Sum calculation with a regression test.\n"}),
        ));
        faux.push(FauxResponse::text(
            "Updated `sum` and added a regression test.",
        ));
        cx.update(|cx| {
            let mut models = Models::new();
            models.set_provider(Arc::new(faux.clone()));
            AppModels::set(models, false, cx);
        });
        let window = cx
            .open_offscreen_window(size(px(width), px(760.)), |window, cx| {
                cx.new(|cx| ProjectWindow::new(root.clone(), window, cx))
            })
            .unwrap();
        pump(cx, Duration::from_millis(200));
        cx.simulate_input(window.into(), "Fix the sum calculation and add a test.");
        cx.dispatch_action(window.into(), SendMessage);
        pump(cx, Duration::from_millis(1800));
        let conversation = cx.update(|cx| {
            window
                .read(cx)
                .unwrap()
                .active_conversation()
                .unwrap()
                .clone()
        });
        let summary = cx.update(|cx| {
            conversation
                .read(cx)
                .turn_changes_for_row(4)
                .expect("completed turn footer")
        });
        assert_eq!((summary.files, summary.added, summary.removed), (3, 6, 1));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(format!("42-turn-changes{suffix}.png")))
            .unwrap();
        window
            .update(cx, |project, window, cx| {
                let items = conversation.update(cx, |view, cx| {
                    view.diff_items(zp_core::changes::Scope::Group(summary.group), cx)
                });
                assert_eq!(items.len(), 3);
                project.open_diff(items, 0, window, cx);
            })
            .unwrap();
        pump(cx, Duration::from_millis(100));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(format!("43-turn-diff{suffix}.png")))
            .unwrap();
        cx.dispatch_action(window.into(), crate::actions::CloseDiff);
        window
            .update(cx, |_, window, cx| {
                conversation.update(cx, |view, cx| {
                    view.revert(
                        zp_core::changes::Scope::Group(summary.group),
                        None,
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
        pump(cx, Duration::from_millis(100));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(format!("45-turn-revert-confirm{suffix}.png")))
            .unwrap();
        cx.simulate_click(
            window.into(),
            point(px(width / 2.), px(440.)),
            Modifiers::none(),
        );
        pump(cx, Duration::from_millis(100));
        assert_eq!(
            std::fs::read_to_string(root.join("math.rs")).unwrap(),
            original
        );
        assert!(!root.join("tests/sum.rs").exists());
        assert!(!root.join("README.md").exists());
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(format!("44-turn-reverted{suffix}.png")))
            .unwrap();
        window
            .update(cx, |project, window, cx| {
                let items = conversation.update(cx, |view, cx| {
                    view.diff_items(zp_core::changes::Scope::Group(summary.group), cx)
                });
                assert!(items.iter().all(|item| item.status
                    == zp_core::changes::RevertStatus::Reverted
                    && item.revert.is_none()));
                project.open_diff(items, 0, window, cx);
            })
            .unwrap();
        pump(cx, Duration::from_millis(100));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(format!("46-reverted-diff{suffix}.png")))
            .unwrap();
    }
}

fn terminal_interactions(
    cx: &mut VisualTestAppContext,
    root: &std::path::Path,
    out: &std::path::Path,
) {
    for (width, name, search_name) in [
        (1000., "38-terminal-links.png", "39-terminal-search.png"),
        (
            700.,
            "40-terminal-links-narrow.png",
            "41-terminal-search-narrow.png",
        ),
    ] {
        let project_root = root.join(format!("terminal-{}", width as u32));
        std::fs::create_dir_all(&project_root).unwrap();
        let root = project_root.as_path();
        let window = cx
            .open_offscreen_window(size(px(width), px(650.)), |window, cx| {
                let session = zp_session::Session::in_memory(root);
                cx.new(|cx| ProjectWindow::new_with_session(root.to_owned(), session, window, cx))
            })
            .unwrap();
        window
            .update(cx, |view, window, cx| {
                view.toggle_terminal(window, cx);
                view.toggle_terminal_maximized(cx);
            })
            .unwrap();
        pump(cx, Duration::from_millis(500));
        let terminal = cx.update(|cx| window.read(cx).unwrap().terminal_for_screenshot().unwrap());
        cx.update(|cx| terminal.update(cx, |view, _| view.write(
            b"printf '%b' '\\033[3J\\033[2J\\033[H\\033[1;32mServer ready\\033[0m\\nhttp://localhost:3000/project/development/preview?theme=dark&view=terminal\\n\\033]8;;https://example.com/docs\\033\\\\Documentation\\033]8;;\\033\\\\\\n\\nBUILD complete: 12 modules\\nbuild cache: 12 hits, 0 misses\\n'; sleep 30\r".to_vec()
        )));
        pump(cx, Duration::from_millis(600));
        let target = cx.update(|cx| terminal.read(cx).cell_position(1, 12));
        let modifiers = if cfg!(target_os = "macos") {
            Modifiers {
                platform: true,
                ..Modifiers::none()
            }
        } else {
            Modifiers {
                control: true,
                ..Modifiers::none()
            }
        };
        cx.simulate_mouse_move(window.into(), target, None, modifiers);
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(name))
            .unwrap();
        cx.simulate_keystrokes(
            window.into(),
            if cfg!(target_os = "macos") {
                "cmd-f"
            } else {
                "ctrl-shift-f"
            },
        );
        cx.simulate_input(window.into(), "build");
        pump(cx, Duration::from_millis(100));
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(search_name))
            .unwrap();
    }
}

fn review_model_dialogs(
    cx: &mut VisualTestAppContext,
    root: &std::path::Path,
    out: &std::path::Path,
) {
    use zp_core::permissions::PermissionMode;

    let window = cx
        .open_offscreen_window(size(px(1000.), px(700.)), |window, cx| {
            let session = zp_session::Session::in_memory(root);
            cx.new(|cx| ProjectWindow::new_with_session(root.to_owned(), session, window, cx))
        })
        .unwrap();
    window
        .update(cx, |project, window, cx| {
            let conversation = project.active_conversation().unwrap().clone();
            conversation.update(cx, |view, cx| {
                view.select_permission_mode(PermissionMode::AutoReview, window, cx);
            });
        })
        .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(window.into())
        .unwrap()
        .save(out.join("35-conversation-review-model.png"))
        .unwrap();
    cx.simulate_keystrokes(window.into(), "enter");
    pump(cx, Duration::from_millis(100));
    window
        .update(cx, |project, _, cx| {
            assert_eq!(
                project
                    .active_conversation()
                    .unwrap()
                    .read(cx)
                    .permission_mode(),
                PermissionMode::AutoReview
            );
        })
        .unwrap();

    let settings = cx
        .open_offscreen_window(size(px(980.), px(700.)), |window, cx| {
            cx.new(|cx| {
                crate::settings::SettingsWindow::new_on(
                    crate::settings::Page::Permissions,
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(settings.into())
        .unwrap()
        .save(out.join("36-permission-settings.png"))
        .unwrap();
    settings
        .update(cx, |view, window, cx| {
            view.set_permission_mode(PermissionMode::AutoReview, window, cx)
        })
        .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(settings.into())
        .unwrap()
        .save(out.join("37-settings-review-model.png"))
        .unwrap();
    cx.simulate_keystrokes(settings.into(), "escape");
    pump(cx, Duration::from_millis(100));
    settings
        .update(cx, |view, _, _| {
            assert!(view.review_model_picker().is_none())
        })
        .unwrap();
}

fn inline_code_layout(
    cx: &mut VisualTestAppContext,
    root: &std::path::Path,
    out: &std::path::Path,
) {
    let markdown = "Staged across 5 files: `shell::all_allowed` now treats navigation as an allowed helper, new `unapproved_prefixes` skips navigation and approved commands, plus `debug_selector` on approval buttons and tests confirm the behavior. `PLAN.md` remains untracked. 后续 `echo` 和 `git status` 也保持对齐。 Wrapped `cargo test -p zp-app selection && cargo check -p zp-app && cargo fmt --all -- --check` then `git status`.";
    let blocks = crate::markdown::parse(markdown);
    let crate::markdown::Block::Paragraph(inline) = &blocks[0] else {
        panic!("expected paragraph")
    };
    let ranges: Vec<_> = inline
        .spans
        .iter()
        .filter(|(_, span)| span.code)
        .map(|(range, _)| range.clone())
        .collect();
    for (width, name) in [
        (1000., "33-inline-code-wide.png"),
        (700., "34-inline-code-narrow.png"),
    ] {
        let window = cx
            .open_offscreen_window(size(px(width), px(650.)), |window, cx| {
                let session = zp_session::Session::in_memory(root);
                cx.new(|cx| ProjectWindow::new_with_session(root.to_owned(), session, window, cx))
            })
            .unwrap();
        let conversation = cx.update(|cx| {
            window
                .read(cx)
                .unwrap()
                .active_conversation()
                .unwrap()
                .clone()
        });
        cx.update(|cx| {
            conversation.update(cx, |view, cx| {
                view.inject(
                    vec![AgentEvent::MessageEnd {
                        message: assistant(vec![AssistantContent::Text(TextContent {
                            text: markdown.into(),
                        })])
                        .into(),
                    }],
                    false,
                    cx,
                );
            })
        });
        pump(cx, Duration::from_millis(100));
        let selection = cx.update(|cx| conversation.read(cx).text_selection());
        selection.assert_inline_geometry(&inline.text, &ranges);
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(name))
            .unwrap();

        let range = ranges.last().unwrap();
        let from = selection.point_for_text(&inline.text, range.start).unwrap();
        let to = selection.point_for_text(&inline.text, range.end).unwrap();
        cx.simulate_mouse_down(window.into(), from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(window.into(), to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(window.into(), to, MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes(window.into(), "secondary-c");
        assert_eq!(
            cx.update(|cx| cx.read_from_clipboard().unwrap().text().unwrap()),
            "git status"
        );
    }
}

/// Writes a session whose entries are `age_ms` old.
fn seed_session(
    data: &std::path::Path,
    root: &std::path::Path,
    messages: &[(&str, &str)],
    age_ms: i64,
) -> PathBuf {
    let manager = zp_session::SessionManager::new(zp_core::paths::sessions_dir(data));
    let mut session = manager.create(root);
    for (question, answer) in messages {
        session
            .append_message(AgentMessage::user(*question))
            .unwrap();
        let mut reply = AssistantMessage::empty("faux", "faux", "faux-1");
        reply.content.push(AssistantContent::Text(TextContent {
            text: (*answer).into(),
        }));
        session.append_message(reply.into()).unwrap();
    }
    let path = session.file().unwrap().to_path_buf();
    shift_timestamps(&path, age_ms);
    path
}

fn shift_timestamps(path: &std::path::Path, age_ms: i64) {
    let text = std::fs::read_to_string(path).unwrap();
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(ts) = value.get("timestamp").and_then(serde_json::Value::as_i64) {
                value["timestamp"] = json!(ts - age_ms);
            }
            value.to_string()
        })
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// M6: several tabs with different states, grouped history, the session tree and the
/// session context menu.
fn tabs_and_history(
    cx: &mut VisualTestAppContext,
    data: &std::path::Path,
    root: &std::path::Path,
    out: &std::path::Path,
) {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    seed_session(
        data,
        root,
        &[(
            "Explain the session file format",
            "Sessions are JSONL trees.",
        )],
        2 * 60 * 60 * 1000,
    );
    seed_session(
        data,
        root,
        &[(
            "Add a --json flag to the CLI",
            "Done: `zp --json` prints events.",
        )],
        DAY,
    );
    seed_session(
        data,
        root,
        &[("Why is the build slow?", "Mostly link time; try mold.")],
        3 * DAY,
    );
    seed_session(
        data,
        root,
        &[(
            "Set up CI on GitHub Actions",
            "Added .github/workflows/ci.yml.",
        )],
        20 * DAY,
    );
    // A session with two branches, for the tree view.
    let branched = seed_session(
        data,
        root,
        &[
            ("Write a tokenizer", "Here is a hand-written lexer."),
            ("Make it faster", "Use a byte table."),
        ],
        5 * 60 * 1000,
    );
    {
        let mut session = zp_session::Session::open(&branched).unwrap();
        let first_answer = session.entries()[1].id.clone();
        session.branch(&first_answer).unwrap();
        session
            .append_message(AgentMessage::user("Use logos instead"))
            .unwrap();
        let mut reply = AssistantMessage::empty("faux", "faux", "faux-1");
        reply.content.push(AssistantContent::Text(TextContent {
            text: "Switched to the logos crate.".into(),
        }));
        session.append_message(reply.into()).unwrap();
    }

    let window = cx
        .open_offscreen_window(size(px(1280.), px(820.)), |window, cx| {
            cx.new(|cx| ProjectWindow::new(root.to_path_buf(), window, cx))
        })
        .unwrap();
    pump(cx, Duration::from_millis(200));
    let any = window.into();
    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        view.update(cx, |view, cx| {
            // A finished background tab (unread), a running one, a failed one, then the
            // branched session in front.
            view.new_tab(window, cx);
            let running = view.active_conversation().unwrap().clone();
            running.update(cx, |c, cx| c.inject(tool_running_events(), true, cx));
            view.new_tab(window, cx);
            let failed = view.active_conversation().unwrap().clone();
            failed.update(cx, |c, cx| c.inject(error_events(), false, cx));
            view.open_session_file(&branched, false, window, cx);
            view.tabs_mut()[0].unread = true;
            view.toggle_pin(0, cx);
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(200));
    // Hover the first message to show its actions.
    cx.simulate_mouse_move(any, point(px(700.), px(118.)), None, Modifiers::none());
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("8-tabs-history.png"))
        .unwrap();

    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        view.update(cx, |view, cx| {
            view.active_conversation()
                .unwrap()
                .update(cx, |c, cx| c.toggle_tree(cx));
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("9-session-tree.png"))
        .unwrap();

    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        view.update(cx, |view, cx| {
            view.active_conversation()
                .unwrap()
                .update(cx, |c, cx| c.toggle_tree(cx));
            let info = view.visible_sessions(cx).into_iter().nth(2).unwrap();
            view.session_menu(&info, point(px(120.), px(200.)), window, cx);
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("10-session-menu.png"))
        .unwrap();
}

fn assistant(blocks: Vec<AssistantContent>) -> AssistantMessage {
    let mut message = AssistantMessage::empty("faux", "faux", "faux-1");
    message.content = blocks;
    message.stop_reason = StopReason::ToolUse;
    message
}

fn tool_running_events() -> Vec<AgentEvent> {
    let call = ToolCall {
        id: "call-1".into(),
        name: "bash".into(),
        arguments: json!({"command": "cargo test --workspace"}),
    };
    let message = assistant(vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "Run the whole test suite to see what breaks.".into(),
            ..ThinkingContent::default()
        }),
        AssistantContent::Text(TextContent {
            text: "I'll run the test suite first.".into(),
        }),
        AssistantContent::ToolCall(call.clone()),
    ]);
    vec![
        AgentEvent::MessageEnd {
            message: AgentMessage::user("Make the tests pass"),
        },
        AgentEvent::MessageStart {
            message: message.clone().into(),
        },
        AgentEvent::MessageEnd {
            message: message.into(),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
        },
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: call.id,
            tool_name: call.name,
            partial_result: AgentToolResult::text(
                "   Compiling zp-ai v0.1.0\n   Compiling zp-agent v0.1.0\n    Finished `test` profile in 4.2s\n     Running unittests src/lib.rs\n\nrunning 61 tests\ntest model::tests::cost_uses_matching_tier ... ok\ntest models::tests::finds_models_by_spec ... ok",
            ),
        },
    ]
}

fn streaming_text_events() -> Vec<AgentEvent> {
    let skeleton = AssistantMessage::empty("faux", "faux", "faux-1");
    let mut events = vec![
        AgentEvent::MessageEnd {
            message: AgentMessage::user("How should I structure the parser?"),
        },
        AgentEvent::MessageStart {
            message: skeleton.into(),
        },
        AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::ThinkingStart {
                index: 0,
                redacted: false,
            },
        },
        AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::ThinkingDelta {
                index: 0,
                delta: "Consider a recursive descent parser…".into(),
            },
        },
        AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::ThinkingEnd {
                index: 0,
                signature: None,
            },
        },
        AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::TextStart { index: 1 },
        },
    ];
    let text = "A **recursive descent** parser fits well here:\n\n- one function per grammar rule\n- errors carry the token position\n\n```rust\nfn parse_expr(&mut self) -> Result<Expr> {\n    let lhs = self.parse_term()?;\n";
    events.push(AgentEvent::MessageUpdate {
        event: AssistantMessageEvent::TextDelta {
            index: 1,
            delta: text.into(),
        },
    });
    events
}

fn error_events() -> Vec<AgentEvent> {
    let mut failed = AssistantMessage::empty("faux", "faux", "faux-1");
    failed.stop_reason = StopReason::Error;
    failed.error_message =
        Some("HTTP 529 Overloaded: the service is temporarily overloaded".into());
    vec![
        AgentEvent::MessageEnd {
            message: AgentMessage::user("Summarize the README"),
        },
        AgentEvent::MessageStart {
            message: failed.clone().into(),
        },
        AgentEvent::MessageEnd {
            message: failed.into(),
        },
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    ]
}

const PARSER: &str = "use crate::lexer::{Lexer, Token};

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    peeked: Option<Token>,
}

impl<'a> Parser<'a> {
    pub fn new(source: &'a str) -> Self {
        Self { lexer: Lexer::new(source), peeked: None }
    }

    /// expr := term (('+' | '-') term)*
    pub fn expr(&mut self) -> Result<i64, String> {
        let mut value = self.term()?;
        while let Some(op) = self.eat_any(&[Token::Plus, Token::Minus]) {
            let rhs = self.term()?;
            value = if op == Token::Plus { value + rhs } else { value - rhs };
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<i64, String> {
        match self.next() {
            Some(Token::Number(n)) => Ok(n),
            Some(Token::LParen) => {
                let value = self.expr()?;
                self.expect(Token::RParen)?;
                Ok(value)
            }
            other => Err(format!(\"unexpected {other:?}\")),
        }
    }
}
";

/// M7: inline diffs, the changes panel and the diff view, after an agent edited files
/// over two turns.
fn changes(cx: &mut VisualTestAppContext, faux: &FauxProvider, out: &std::path::Path) {
    let project = tempfile::tempdir().unwrap();
    let root = zp_core::project::normalize_project_path(project.path()).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/parser.rs"), PARSER).unwrap();
    std::fs::write(
        root.join("README.md"),
        "# calc\n\nA tiny calculator.\n\n## Usage\n\n    calc '1 + 2'\n",
    )
    .unwrap();

    faux.push(FauxResponse::blocks(vec![
        FauxBlock::Text("Negative numbers fail in `term`; I'll accept a leading minus.".into()),
        FauxBlock::ToolCall {
            name: "edit".into(),
            arguments: json!({"path": "src/parser.rs", "edits": [{
                "oldText": "            Some(Token::Number(n)) => Ok(n),\n",
                "newText": "            Some(Token::Number(n)) => Ok(n),\n            // Unary minus: `-3`, `-(1 + 2)`.\n            Some(Token::Minus) => Ok(-self.term()?),\n"
            }]}),
        },
    ]));
    faux.push(FauxResponse::tool_call(
        "write",
        json!({"path": "tests/negative.rs", "content": "use calc::Parser;\n\n#[test]\nfn negative_numbers() {\n    assert_eq!(Parser::new(\"-3 + 5\").expr(), Ok(2));\n    assert_eq!(Parser::new(\"-(1 + 2)\").expr(), Ok(-3));\n}\n"}),
    ));
    faux.push(FauxResponse::text(
        "Done: `term` now handles a leading `-`, with tests.",
    ));
    faux.push(FauxResponse::tool_call(
        "edit",
        json!({"path": "README.md", "edits": [{
            "oldText": "    calc '1 + 2'\n",
            "newText": "    calc '1 + 2'\n    calc '-3 + 5'   # negative numbers work too\n"
        }]}),
    ));
    faux.push(FauxResponse::text(
        "The README mentions negative numbers now.",
    ));

    let window = cx
        .open_offscreen_window(size(px(1440.), px(860.)), |window, cx| {
            cx.new(|cx| ProjectWindow::new(root.clone(), window, cx))
        })
        .unwrap();
    let any = window.into();
    pump(cx, Duration::from_millis(200));
    cx.dispatch_action(any, crate::actions::ToggleRightPanel);
    for prompt in [
        "Negative numbers like -3 don't parse",
        "Mention it in the README",
    ] {
        cx.simulate_input(any, prompt);
        pump(cx, Duration::from_millis(50));
        cx.dispatch_action(any, SendMessage);
        pump(cx, Duration::from_millis(4000));
    }
    // Show the first turn's edit.
    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        let conversation = view.read(cx).active_conversation().unwrap().clone();
        conversation.update(cx, |c, _| c.scroll_to_row(1));
    })
    .unwrap();
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("11-inline-diff-and-changes.png"))
        .unwrap();

    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        let panel = view.read(cx).changes_panel().clone();
        panel.update(cx, |panel, cx| {
            panel.set_mode(crate::changes_panel::ChangesMode::Turns, cx)
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(100));
    // Hover the newest turn to show its actions.
    cx.simulate_mouse_move(any, point(px(1300.), px(98.)), None, Modifiers::none());
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("12-changes-by-turn.png"))
        .unwrap();

    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        let panel = view.read(cx).changes_panel().clone();
        panel.update(cx, |panel, cx| {
            panel.open_file(
                zp_core::changes::Scope::Session,
                &root.join("src/parser.rs"),
                cx,
            )
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("13-diff-unified.png"))
        .unwrap();
    cx.dispatch_action(any, crate::actions::ToggleDiffLayout);
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("14-diff-split.png"))
        .unwrap();
}

/// M8: approval cards for a destructive command and a file change, and the permission
/// mode picker.
fn approvals(cx: &mut VisualTestAppContext, faux: &FauxProvider, out: &std::path::Path) {
    let project = tempfile::tempdir().unwrap();
    let root = zp_core::project::normalize_project_path(project.path()).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/config.rs"),
        "pub const RETRIES: u32 = 3;\npub const TIMEOUT_SECS: u64 = 30;\n",
    )
    .unwrap();
    cx.update(|cx| {
        let root = root.clone();
        crate::store::AppStore::update(cx, move |state| {
            state.project_mut(&root).permission_mode =
                Some(zp_core::permissions::PermissionMode::AutoReview);
            state.project_mut(&root).review_model = Some(faux.model().qualified_id());
        });
    });
    faux.push(FauxResponse::blocks(vec![
        FauxBlock::Text("The build directory is stale; I'll remove it and rebuild.".into()),
        FauxBlock::ToolCall {
            name: "bash".into(),
            arguments: json!({"command": "rm -rf build && cargo build --release"}),
        },
    ]));
    faux.push(FauxResponse::tool_call(
        "edit",
        json!({"path": "/etc/zp-demo.conf", "edits": [{"oldText": "a", "newText": "b"}]}),
    ));

    let window = cx
        .open_offscreen_window(size(px(1280.), px(820.)), |window, cx| {
            cx.new(|cx| ProjectWindow::new(root.clone(), window, cx))
        })
        .unwrap();
    let any = window.into();
    pump(cx, Duration::from_millis(200));
    cx.simulate_input(any, "The release build is broken, rebuild from scratch");
    pump(cx, Duration::from_millis(50));
    cx.dispatch_action(any, SendMessage);
    pump(cx, Duration::from_millis(2500));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("15-approval-command.png"))
        .unwrap();

    // Deny with a reason; the next call writes outside the project.
    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        let conversation = view.read(cx).active_conversation().unwrap().clone();
        conversation.update(cx, |c, cx| {
            c.deny_first_approval("keep build/, run cargo clean instead", cx)
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(2500));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("16-approval-outside.png"))
        .unwrap();

    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        let conversation = view.read(cx).active_conversation().unwrap().clone();
        conversation.update(cx, |c, cx| {
            c.toggle_picker(crate::conversation::Picker::Permissions, window, cx)
        });
    })
    .unwrap();
    pump(cx, Duration::from_millis(100));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("17-permission-modes.png"))
        .unwrap();
}

fn transcript_display_settings(cx: &mut VisualTestAppContext, out: &std::path::Path) {
    use crate::settings::{Page, SettingsWindow};
    use zp_core::settings::ThemeChoice;

    for (name, theme) in [("dark", ThemeChoice::Dark), ("light", ThemeChoice::Light)] {
        cx.update(|cx| {
            crate::store::AppSettings::update(cx, |settings| settings.theme = Some(theme)).unwrap();
        });
        for width in [980., 720.] {
            let window = cx
                .open_offscreen_window(size(px(width), px(700.)), |window, cx| {
                    cx.new(|cx| SettingsWindow::new_on(Page::Appearance, window, cx))
                })
                .unwrap();
            pump(cx, Duration::from_millis(150));
            cx.capture_screenshot(window.into())
                .unwrap()
                .save(out.join(format!("transcript-display-{name}-{width}.png")))
                .unwrap();
        }
    }
}

/// M9: every settings page, with a few real providers registered.
fn settings_pages(cx: &mut VisualTestAppContext, out: &std::path::Path) {
    use crate::settings::{Page, SettingsWindow};
    cx.update(|cx| {
        let app = cx.global::<AppModels>();
        for provider in Models::builtin().providers() {
            app.models.register(provider);
        }
        app.credentials.set("deepseek", "sk-demo-key").unwrap();
        crate::store::AppSettings::update(cx, |settings| {
            settings.default_model = Some("deepseek/deepseek-v4-pro".into());
            settings.providers = vec![zp_core::settings::CustomProvider {
                id: "ollama".into(),
                name: "Ollama".into(),
                base_url: "http://localhost:11434/v1".into(),
                models: vec!["qwen3-coder:30b".into(), "gpt-oss:20b".into()],
                ..Default::default()
            }];
            settings
                .keybindings
                .insert("zp::SendMessage".into(), "cmd-enter".into());
        })
        .unwrap();
        zp_core::providers::register_custom_providers(
            &cx.global::<AppModels>().models,
            &crate::store::AppSettings::get(cx).providers,
        );
    });
    for (index, page) in Page::ALL.into_iter().enumerate() {
        let window = cx
            .open_offscreen_window(size(px(980.), px(700.)), |window, cx| {
                cx.new(|cx| SettingsWindow::new_on(page, window, cx))
            })
            .unwrap();
        pump(cx, Duration::from_millis(150));
        let name = format!(
            "{}-settings-{}.png",
            18 + index,
            page.label().to_lowercase()
        );
        cx.capture_screenshot(window.into())
            .unwrap()
            .save(out.join(name))
            .unwrap();
        if page == Page::Providers {
            let shot = |cx: &mut VisualTestAppContext, name: &str| {
                pump(cx, Duration::from_millis(150));
                cx.capture_screenshot(window.into())
                    .unwrap()
                    .save(out.join(name))
                    .unwrap();
            };
            let step = |cx: &mut VisualTestAppContext,
                        f: &dyn Fn(
                &mut SettingsWindow,
                &mut gpui::Window,
                &mut gpui::Context<SettingsWindow>,
            )| {
                cx.update_window(window.into(), |_, window, cx| {
                    let view = window.root::<SettingsWindow>().flatten().unwrap();
                    view.update(cx, |view, cx| f(view, window, cx));
                })
                .unwrap();
            };
            step(cx, &|view, window, cx| view.open_add_dialog(window, cx));
            shot(cx, "24-settings-add-provider.png");
            step(cx, &|view, window, cx| view.edit_key("openai", window, cx));
            cx.update_window(window.into(), |_, window, cx| {
                let view = window.root::<SettingsWindow>().flatten().unwrap();
                if let Some(field) = view.read(cx).key_input() {
                    field.update(cx, |input, cx| {
                        input.emplace("sk-proj-0123456789abcdef", cx)
                    });
                }
            })
            .unwrap();
            shot(cx, "25-settings-api-key.png");
            step(cx, &|view, window, cx| {
                view.open_provider_form(None, window, cx)
            });
            shot(cx, "30-settings-custom-provider.png");
        }
    }
}

/// M10: the `/` and `@` menus with an attachment, the compaction card, the command
/// palette and the MCP settings page.
fn advanced(
    cx: &mut VisualTestAppContext,
    faux: &FauxProvider,
    data: &std::path::Path,
    out: &std::path::Path,
) {
    use crate::settings::{Page, SettingsWindow};
    let write = |path: PathBuf, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        data.join("prompts/review.md"),
        "---\ndescription: Review the staged changes\nargument-hint: \"[focus]\"\n---\nReview ${1:-everything}.",
    );
    write(
        data.join("prompts/changelog.md"),
        "---\ndescription: Draft a changelog entry from recent commits\n---\nWrite a changelog.",
    );
    write(
        data.join("skills/release/SKILL.md"),
        "---\nname: release\ndescription: Cut a release: bump versions, tag, publish\n---\nSteps",
    );
    let script = r#"while IFS= read -r line; do id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); case "$line" in *initialize*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id";; *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search_docs","description":"Search the product docs"},{"name":"get_page"}]}}\n' "$id";; esac; done"#;
    std::fs::write(
        zp_mcp::config::user_file(data),
        json!({"mcpServers": {
            "docs": {"command": "sh", "args": ["-c", script], "description": "Product documentation"},
            "github": {"command": "github-mcp-server", "args": ["stdio"]},
            "old": {"enabled": false, "url": "https://example.com/mcp"}
        }})
        .to_string(),
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    let root = zp_core::project::normalize_project_path(project.path()).unwrap();
    for file in ["src/main.rs", "src/parser.rs", "src/lexer.rs", "README.md"] {
        write(root.join(file), "// code\n");
    }
    std::fs::write(root.join("screenshot.png"), [0x89, b'P', b'N', b'G']).unwrap();
    cx.update(|cx| {
        let root = root.clone();
        crate::store::AppStore::update(cx, move |state| {
            state.project_mut(&root).permission_mode =
                Some(zp_core::permissions::PermissionMode::FullAuto);
        });
        // The settings scene made a real provider the default; only the scripted one
        // may answer here.
        let _ = cx.global::<AppModels>().credentials.remove("deepseek");
        crate::store::AppSettings::update(cx, |settings| {
            settings.default_model = Some("faux/faux-1".into());
            settings.keybindings.clear();
            settings.generate_titles = Some(false);
        })
        .unwrap();
    });
    let window = cx
        .open_offscreen_window(size(px(1280.), px(820.)), |window, cx| {
            cx.new(|cx| ProjectWindow::new(root.clone(), window, cx))
        })
        .unwrap();
    let any = window.into();
    pump(cx, Duration::from_millis(300));
    let conversation = cx.update(|cx| {
        window
            .read(cx)
            .map(|v| v.active_conversation().unwrap().clone())
            .unwrap()
    });

    // Two turns, then a compaction.
    faux.push(FauxResponse::text(
        "The parser is a recursive descent parser in src/parser.rs.",
    ));
    faux.push(FauxResponse::text("Errors carry the token position."));
    faux.push(FauxResponse::text("## Goal\nUnderstand the parser.\n\n## Progress\n### Done\n- [x] Read src/parser.rs\n\n## Next Steps\n1. Improve error messages"));
    for question in ["How does the parser work?", "And error handling?"] {
        cx.simulate_input(any, question);
        cx.dispatch_action(any, SendMessage);
        pump(cx, Duration::from_millis(1500));
    }
    cx.update(|cx| conversation.update(cx, |c, cx| c.compact(None, cx)));
    pump(cx, Duration::from_millis(800));
    cx.update(|cx| conversation.update(cx, |c, cx| c.toggle_tool("compaction-0", 0, false, cx)));
    pump(cx, Duration::from_millis(100));

    cx.simulate_input(any, "/");
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("26-slash-commands.png"))
        .unwrap();

    // A draft longer than one line wraps the same way in every frame.
    cx.update(|cx| conversation.update(cx, |c, cx| c.set_draft("", cx)));
    cx.simulate_input(
        any,
        "Please refactor the parser so that every grammar rule gets its own function, keep the error positions, and add tests for nested parentheses and unary minus, then run the whole test suite.",
    );
    for frame in 0..3 {
        pump(cx, Duration::from_millis(250));
        cx.capture_screenshot(any)
            .unwrap()
            .save(out.join(format!("33-long-draft-{frame}.png")))
            .unwrap();
    }

    cx.update(|cx| conversation.update(cx, |c, cx| c.set_draft("", cx)));
    cx.update_window(any, |_, window, cx| {
        conversation.update(cx, |c, cx| {
            let paths = gpui::ExternalPaths([root.join("screenshot.png")].into_iter().collect());
            c.drop_paths(&paths, window, cx);
        });
    })
    .unwrap();
    cx.simulate_input(any, "explain the errors in @pars");
    pump(cx, Duration::from_millis(600));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("27-file-completion.png"))
        .unwrap();

    cx.update(|cx| conversation.update(cx, |c, cx| c.set_draft("", cx)));
    cx.update_window(any, |_, window, cx| {
        let view = window.root::<ProjectWindow>().flatten().unwrap();
        view.update(cx, |view, cx| view.toggle_palette(window, cx));
    })
    .unwrap();
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("28-command-palette.png"))
        .unwrap();

    let settings = cx
        .open_offscreen_window(size(px(980.), px(700.)), |window, cx| {
            cx.new(|cx| SettingsWindow::new_on(Page::Mcp, window, cx))
        })
        .unwrap();
    pump(cx, Duration::from_millis(800));
    cx.capture_screenshot(settings.into())
        .unwrap()
        .save(out.join("29-settings-mcp.png"))
        .unwrap();
}

/// The Git view: staged and unstaged files, history and the branch picker.
fn git_panel(cx: &mut VisualTestAppContext, out: &std::path::Path) {
    let project = tempfile::tempdir().unwrap();
    let root = zp_core::project::normalize_project_path(project.path()).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["-c", "init.defaultBranch=main"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    for (key, value) in [
        ("user.name", "Ada"),
        ("user.email", "ada@example.com"),
        ("commit.gpgsign", "false"),
    ] {
        git(&["config", key, value]);
    }
    std::fs::create_dir_all(root.join("crates/zp-app/src")).unwrap();
    std::fs::write(root.join("README.md"), "# calc\n").unwrap();
    std::fs::write(
        root.join("crates/zp-app/src/tests_advanced.rs"),
        "fn one() {}\n".repeat(12),
    )
    .unwrap();
    std::fs::write(root.join("crates/zp-app/src/main.rs"), "fn main() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "Initial commit"]);
    let remote = tempfile::tempdir().unwrap();
    let remote_path = zp_core::project::normalize_project_path(remote.path()).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote_path)
        .output()
        .unwrap();
    assert!(init.status.success());
    git(&["remote", "add", "origin", remote_path.to_str().unwrap()]);
    git(&["push", "-q", "-u", "origin", "main"]);
    for (index, summary) in [
        "Parse negative numbers",
        "Add a README",
        "Fix the IME window position",
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(root.join("notes.txt"), format!("{index}\n")).unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", summary]);
    }
    git(&["branch", "feature/history"]);
    std::fs::write(
        root.join("crates/zp-app/src/tests_advanced.rs"),
        "fn two() {}\n".repeat(19),
    )
    .unwrap();
    git(&["add", "crates/zp-app/src/tests_advanced.rs"]);
    std::fs::write(
        root.join("crates/zp-app/src/main.rs"),
        "fn main() {\n    run();\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("PLAN.md"),
        "# On the Value of ‘Slowness’\n\n**Abstract**: This article discusses the long-term benefits of deliberately slowing down the pace in fast-paced software development, including less rework, clearer designs, and a deeper shared understanding of the codebase amongst the team. This line has been deliberately made long to test horizontal scrolling.\n**Keywords**: pace, quality, maintainability\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("README.md")).unwrap();

    let window = cx
        .open_offscreen_window(size(px(1280.), px(820.)), |window, cx| {
            cx.new(|cx| ProjectWindow::new(root.clone(), window, cx))
        })
        .unwrap();
    let any = window.into();
    pump(cx, Duration::from_millis(200));
    cx.dispatch_action(any, crate::actions::ToggleRightPanel);
    pump(cx, Duration::from_millis(800));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("40-git-changes.png"))
        .unwrap();

    let panel = cx
        .update_window(any, |_, window, cx| {
            let view = window.root::<ProjectWindow>().flatten().unwrap();
            view.read(cx).changes_panel().read(cx).git().clone()
        })
        .unwrap();
    cx.update_window(any, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.commit_menu(point(px(1180.), px(700.)), window, cx)
        })
    })
    .unwrap();
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("43-git-commit-options.png"))
        .unwrap();
    cx.update_window(any, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.toggle_signoff(cx);
            panel.toggle_amend(cx);
        });
        let _ = window;
    })
    .unwrap();
    pump(cx, Duration::from_millis(400));
    cx.simulate_click(any, point(px(1260.), px(641.)), Modifiers::none());
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("44-git-amend-maximized.png"))
        .unwrap();
    cx.simulate_click(any, point(px(1265.), px(160.)), Modifiers::none());
    cx.update_window(any, |_, _, cx| {
        panel.update(cx, |panel, cx| {
            panel.toggle_signoff(cx);
            panel.toggle_amend(cx);
        })
    })
    .unwrap();
    pump(cx, Duration::from_millis(200));

    cx.update_window(any, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.open_branch_picker(window, cx))
    })
    .unwrap();
    pump(cx, Duration::from_millis(500));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("41-git-branches.png"))
        .unwrap();

    cx.update_window(any, |_, _, cx| {
        panel.update(cx, |panel, cx| {
            panel.close_branch_picker(cx);
            panel.set_tab(crate::git_panel::GitTab::History, cx);
        })
    })
    .unwrap();
    pump(cx, Duration::from_millis(500));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("42-git-history.png"))
        .unwrap();

    // A new file's diff: no old line numbers, long lines scroll sideways, selected text.
    cx.update_window(any, |_, _, cx| {
        panel.update(cx, |panel, cx| {
            panel.set_tab(crate::git_panel::GitTab::Changes, cx);
            let file = panel
                .status()
                .unwrap()
                .files
                .iter()
                .find(|f| f.repo_path == "PLAN.md")
                .unwrap()
                .clone();
            panel.open_file(&file, crate::git_panel::Side::Unstaged, cx);
        })
    })
    .unwrap();
    pump(cx, Duration::from_millis(500));
    cx.simulate_mouse_down(
        any,
        point(px(300.), px(180.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.simulate_mouse_move(
        any,
        point(px(560.), px(220.)),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.simulate_mouse_up(
        any,
        point(px(560.), px(220.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("45-git-diff-new-file.png"))
        .unwrap();
    cx.simulate_event(
        any,
        gpui::ScrollWheelEvent {
            position: point(px(600.), px(300.)),
            delta: gpui::ScrollDelta::Pixels(point(px(-400.), px(0.))),
            modifiers: Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        },
    );
    pump(cx, Duration::from_millis(200));
    cx.capture_screenshot(any)
        .unwrap()
        .save(out.join("46-git-diff-scrolled.png"))
        .unwrap();
}
