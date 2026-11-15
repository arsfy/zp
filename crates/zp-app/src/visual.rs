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

use gpui::{AppContext as _, Modifiers, VisualTestAppContext, point, px, size};
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
    tabs_and_history(&mut cx, &data_dir_of(&data), &root, &out);
    println!("screenshots written to {}", out.display());
}

fn data_dir_of(data: &tempfile::TempDir) -> PathBuf {
    data.path().to_path_buf()
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
