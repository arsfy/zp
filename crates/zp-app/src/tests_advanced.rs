//! M10: `/` commands, prompt templates and skills, `@file` references and images,
//! compaction, the command palette, notifications, MCP tools and update checks.

use gpui::{ClipboardItem, Image, ImageFormat, TestAppContext};
use serde_json::json;
use zp_ai::{FauxProvider, FauxResponse, Message, UserContent};

use crate::conversation::composer::CompletionKind;
use crate::tests::project_dir;
use crate::tests_conversation::{active, idle, open, send, setup_models, tab_count, wait_until};

/// The text of the last user message the provider received.
fn last_prompt(faux: &FauxProvider) -> Vec<UserContent> {
    let request = faux.requests().last().cloned().expect("a request");
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .unwrap()
}

fn text_of(content: &[UserContent]) -> String {
    content.iter().filter_map(UserContent::as_text).collect()
}

fn write(path: std::path::PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn set_draft(
    window: gpui::WindowHandle<crate::project::ProjectWindow>,
    text: &str,
    cx: &mut TestAppContext,
) {
    let conversation = active(window, cx);
    conversation.update(cx, |view, cx| view.set_draft(text, cx));
    cx.run_until_parked();
}

#[gpui::test]
fn commands_templates_and_skills(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("reviewed"));
    faux.push(FauxResponse::text("used the skill"));
    let data = setup_models(cx, &[&faux], false);
    write(
        data.path().join("prompts/review.md"),
        "---\ndescription: Review a part\nargument-hint: \"[area]\"\n---\nReview ${1:-everything} carefully.",
    );
    write(
        data.path().join("skills/release/SKILL.md"),
        "---\nname: release\ndescription: Cut a release\n---\nRun the release checklist.",
    );
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);

    // The skill is advertised to the model.
    let prompt = conversation.read_with(cx, |v, _| v.system_prompt());
    assert!(prompt.contains("<name>release</name>"));

    // Typing `/re` offers commands and templates; Tab completes.
    set_draft(window, "/re", cx);
    let labels: Vec<String> = conversation.read_with(cx, |view, _| {
        let completion = view.completion().unwrap();
        assert_eq!(completion.kind, CompletionKind::Command);
        completion.items.iter().map(|i| i.label.clone()).collect()
    });
    assert_eq!(labels, ["/reload", "/review [area]"]);
    cx.dispatch_action(window.into(), crate::actions::CompletionNext);
    cx.dispatch_action(window.into(), crate::actions::AcceptCompletion);
    cx.run_until_parked();
    assert_eq!(conversation.read_with(cx, |v, cx| v.draft(cx)), "/review ");
    assert!(conversation.read_with(cx, |v, _| v.completion().is_none()));

    // Templates expand with their arguments.
    send(window, "/review \"the parser\"", cx);
    idle(&conversation, 2, cx);
    assert_eq!(text_of(&last_prompt(&faux)), "Review the parser carefully.");
    cx.simulate_keystrokes(window.into(), "up");
    assert_eq!(
        conversation.read_with(cx, |view, cx| view.draft(cx)),
        "/review \"the parser\""
    );
    assert!(conversation.read_with(cx, |view, _| view.completion().is_none()));
    cx.simulate_keystrokes(window.into(), "down");

    // `/skill:name` loads the skill.
    send(window, "/skill:release now", cx);
    idle(&conversation, 4, cx);
    let text = text_of(&last_prompt(&faux));
    assert!(text.starts_with("<skill name=\"release\""), "{text}");
    assert!(text.ends_with("Run the release checklist.\n</skill>\n\nnow"));

    // Built-in commands run locally.
    let requests = faux.requests().len();
    send(window, "/help", cx);
    let info = conversation.read_with(cx, |v, _| v.info_text()).unwrap();
    assert!(info.contains("/compact [focus]") && info.contains("/review, /skill:release"));
    send(window, "/new", cx);
    assert_eq!(tab_count(window, cx), 2);
    assert_eq!(faux.requests().len(), requests);

    // Escape closes the menu until the text changes.
    let conversation = active(window, cx);
    set_draft(window, "/", cx);
    assert!(conversation.read_with(cx, |v, _| v.completion().is_some()));
    cx.dispatch_action(window.into(), crate::actions::CloseCompletion);
    cx.run_until_parked();
    assert!(conversation.read_with(cx, |v, _| v.completion().is_none()));
}

#[gpui::test]
fn option_commands_choose_in_the_completion_menu(cx: &mut TestAppContext) {
    let a = FauxProvider::new("faux-a");
    let b = FauxProvider::new("faux-b");
    let _data = setup_models(cx, &[&a, &b], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    let labels = |cx: &mut TestAppContext| -> Vec<String> {
        conversation.read_with(cx, |view, _| {
            let completion = view.completion().unwrap();
            assert_eq!(completion.kind, CompletionKind::Option);
            completion.items.iter().map(|i| i.label.clone()).collect()
        })
    };

    // Completing `/thinking` lists the levels, the current one selected.
    set_draft(window, "/thin", cx);
    cx.dispatch_action(window.into(), crate::actions::SendMessage);
    cx.run_until_parked();
    assert_eq!(
        conversation.read_with(cx, |v, cx| v.draft(cx)),
        "/thinking "
    );
    assert!(labels(cx).contains(&"high".to_owned()));
    let current = conversation.read_with(cx, |v, _| {
        let completion = v.completion().unwrap();
        completion.items[completion.selected].current
    });
    assert!(current);

    // Typing filters; Enter applies and clears the draft.
    set_draft(window, "/thinking hi", cx);
    assert_eq!(labels(cx), ["high"]);
    cx.dispatch_action(window.into(), crate::actions::SendMessage);
    cx.run_until_parked();
    assert_eq!(conversation.read_with(cx, |v, cx| v.draft(cx)), "");
    assert_eq!(
        conversation.read_with(cx, |v, _| v.status().thinking),
        zp_ai::ThinkingLevel::High
    );
    assert!(conversation.read_with(cx, |v, _| v.completion().is_none()));

    // Enter on a bare `/model` opens the list instead of a popover.
    send(window, "/model", cx);
    assert_eq!(conversation.read_with(cx, |v, cx| v.draft(cx)), "/model ");
    assert_eq!(labels(cx).len(), 2);
    set_draft(window, "/model faux-b", cx);
    assert_eq!(labels(cx), ["faux-b/faux-1"]);
    cx.dispatch_action(window.into(), crate::actions::AcceptCompletion);
    cx.run_until_parked();
    let model = conversation.read_with(cx, |v, _| v.status().model.unwrap().qualified_id());
    assert_eq!(model, "faux-b/faux-1");

    // `/permissions` works the same; an argument applies directly.
    send(window, "/permissions", cx);
    assert_eq!(labels(cx).len(), 4);
    send(window, "/permissions full-auto", cx);
    assert_eq!(
        conversation.read_with(cx, |v, _| v.permission_mode()),
        zp_core::permissions::PermissionMode::FullAuto
    );
    assert_eq!(conversation.read_with(cx, |v, cx| v.draft(cx)), "");
}

#[gpui::test]
fn file_references_and_images(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("seen"));
    faux.push(FauxResponse::text("nice picture"));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    write(project.join("src/main.rs"), "fn main() {}\n");
    write(project.join("README.md"), "# Demo\n");
    std::fs::write(project.join("shot.png"), [0x89, b'P', b'N', b'G']).unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);

    // `@ma` completes project files (listed in the background).
    set_draft(window, "explain @ma", cx);
    wait_until(cx, |cx| {
        conversation.read_with(cx, |v, _| v.completion().is_some())
    });
    let first = conversation.read_with(cx, |v, _| {
        let completion = v.completion().unwrap();
        assert_eq!(completion.kind, CompletionKind::File);
        completion.items[0].label.clone()
    });
    assert_eq!(first, "src/main.rs");
    cx.dispatch_action(window.into(), crate::actions::SendMessage); // Enter accepts
    cx.run_until_parked();
    assert_eq!(
        conversation.read_with(cx, |v, cx| v.draft(cx)),
        "explain @src/main.rs "
    );
    cx.dispatch_action(window.into(), crate::actions::SendMessage);
    idle(&conversation, 2, cx);
    assert_eq!(
        text_of(&last_prompt(&faux)),
        "<file name=\"src/main.rs\">\nfn main() {}\n</file>\nexplain @src/main.rs"
    );

    // Dropped images are attached, other files referenced; pasted images attached.
    window
        .update(cx, |_, window, cx| {
            conversation.update(cx, |view, cx| {
                let paths = gpui::ExternalPaths(
                    [project.join("shot.png"), project.join("README.md")]
                        .into_iter()
                        .collect(),
                );
                view.drop_paths(&paths, window, cx)
            })
        })
        .unwrap();
    assert_eq!(
        conversation.read_with(cx, |v, cx| v.draft(cx)),
        "@README.md "
    );
    cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
        ImageFormat::Png,
        vec![1, 2, 3],
    )));
    cx.simulate_keystrokes(window.into(), "secondary-v");
    cx.run_until_parked();
    let names: Vec<String> = conversation.read_with(cx, |v, _| {
        v.attachments().iter().map(|a| a.name.clone()).collect()
    });
    assert_eq!(names, ["shot.png", "Pasted image"]);
    // Plain text still pastes into the field.
    cx.write_to_clipboard(ClipboardItem::new_string("what is this?".into()));
    cx.simulate_keystrokes(window.into(), "secondary-v");
    cx.run_until_parked();
    assert_eq!(
        conversation.read_with(cx, |v, cx| v.draft(cx)),
        "@README.md what is this?"
    );
    cx.dispatch_action(window.into(), crate::actions::SendMessage);
    idle(&conversation, 4, cx);
    let content = last_prompt(&faux);
    assert!(
        text_of(&content)
            .starts_with("<file name=\"README.md\">\n# Demo\n</file>\n@README.md what is this?")
    );
    let images = content
        .iter()
        .filter(|c| matches!(c, UserContent::Image(_)))
        .count();
    assert_eq!(images, 2);
    assert!(conversation.read_with(cx, |v, _| v.attachments().is_empty()));
}

fn compactions(
    conversation: &gpui::Entity<crate::conversation::ConversationView>,
    cx: &mut TestAppContext,
) -> usize {
    conversation.read_with(cx, |view, _| view.compaction_count())
}

fn wait_quiet(
    conversation: &gpui::Entity<crate::conversation::ConversationView>,
    cx: &mut TestAppContext,
) {
    wait_until(cx, |cx| {
        conversation.read_with(cx, |v, _| !v.is_running() && !v.is_compacting())
    });
}

#[gpui::test]
fn manual_compaction_and_overflow_recovery(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    for response in [
        FauxResponse::text("answer one"),
        FauxResponse::text("answer two"),
        FauxResponse::text("## Goal\nManual summary"),
        FauxResponse::Error("400: prompt is too long: 300000 tokens > 200000 maximum".into()),
        FauxResponse::text("## Goal\nSecond summary"),
        FauxResponse::text("recovered answer"),
    ] {
        faux.push(response);
    }
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "question one", cx);
    idle(&conversation, 2, cx);
    send(window, "question two", cx);
    idle(&conversation, 4, cx);

    // `/compact` summarizes the first turn; the second stays verbatim.
    send(window, "/compact keep the API", cx);
    wait_quiet(&conversation, cx);
    assert_eq!(compactions(&conversation, cx), 1);
    let info = conversation.read_with(cx, |v, _| v.info_text()).unwrap();
    assert!(info.starts_with("Compacted the conversation"), "{info}");
    let summary_request = text_of(&last_prompt(&faux));
    assert!(summary_request.contains("[User]: question one"));
    assert!(!summary_request.contains("question two"));
    assert!(summary_request.ends_with("Additional focus: keep the API"));
    let rows = conversation.read_with(cx, |v, _| v.transcript().len());
    assert_eq!(rows, 3, "the summary, then the kept turn");

    // A request too large for the context is compacted and retried once.
    send(window, "question three", cx);
    wait_until(cx, |_| faux.pending() == 0);
    wait_quiet(&conversation, cx);
    assert_eq!(compactions(&conversation, cx), 2);
    let requests = faux.requests();
    assert_eq!(requests.len(), 6);
    let second_summary = match &requests[4].messages[0] {
        Message::User(user) => text_of(&user.content),
        _ => panic!(),
    };
    assert!(second_summary.contains("<previous-summary>\n## Goal\nManual summary"));
    let last = conversation.read_with(cx, |v, _| {
        let transcript = v.transcript();
        transcript.assistant(transcript.len() - 1).map(|m| m.text())
    });
    assert_eq!(last.as_deref(), Some("recovered answer"));
    assert!(!conversation.read_with(cx, |v, _| v.has_failed()));
}

#[gpui::test]
fn automatic_compaction_before_a_prompt(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    for response in [
        FauxResponse::text("a1"),
        FauxResponse::text("a2"),
        FauxResponse::text("## Goal\nAuto summary"),
        FauxResponse::text("a3"),
    ] {
        faux.push(response);
    }
    let data = setup_models(cx, &[&faux], false);
    // Leave no room in the window, so every prompt compacts first.
    let mut settings =
        zp_core::settings::Settings::load(&zp_core::paths::settings_file(data.path())).unwrap();
    settings.compaction = Some(zp_core::compaction::CompactionSettings {
        reserve_tokens: 1_000_000,
        ..Default::default()
    });
    settings.generate_titles = Some(false);
    settings
        .save(&zp_core::paths::settings_file(data.path()))
        .unwrap();
    cx.update(|cx| crate::store::AppSettings::update(cx, |_| {}).unwrap());
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    for (question, rows) in [("q1", 2), ("q2", 4)] {
        send(window, question, cx);
        wait_quiet(&conversation, cx);
        assert_eq!(
            conversation.read_with(cx, |v, _| v.transcript().len()),
            rows
        );
    }
    assert_eq!(
        compactions(&conversation, cx),
        0,
        "nothing older than the last turn yet"
    );
    send(window, "q3", cx);
    wait_until(cx, |_| faux.pending() == 0);
    wait_quiet(&conversation, cx);
    assert_eq!(compactions(&conversation, cx), 1);
    let requests = faux.requests();
    assert!(
        text_of(match &requests[2].messages[0] {
            Message::User(u) => &u.content,
            _ => panic!(),
        })
        .contains("[User]: q1")
    );
    assert_eq!(text_of(&last_prompt(&faux)), "q3");
}

#[gpui::test]
fn command_palette_runs_commands(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    cx.simulate_keystrokes(window.into(), "secondary-shift-p");
    cx.run_until_parked();
    let palette = window
        .update(cx, |v, _, _| v.palette())
        .unwrap()
        .expect("open");
    let filter = palette.read_with(cx, |p, _| p.filter().clone());
    filter.update(cx, |input, cx| input.emplace("new ch", cx));
    let first = palette.read_with(cx, |p, cx| p.matches(cx)[0].1);
    assert_eq!(first, "New Chat");
    cx.simulate_keystrokes(window.into(), "enter");
    cx.run_until_parked();
    assert_eq!(tab_count(window, cx), 2);
    assert!(window.update(cx, |v, _, _| v.palette()).unwrap().is_none());
    // Escape closes it.
    cx.simulate_keystrokes(window.into(), "secondary-shift-p");
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "escape");
    cx.run_until_parked();
    assert!(window.update(cx, |v, _, _| v.palette()).unwrap().is_none());
}

#[gpui::test]
fn notifications_for_background_tabs(cx: &mut TestAppContext) {
    let faux = FauxProvider::default().with_chunk_delay(std::time::Duration::from_millis(30));
    faux.push(FauxResponse::text("done in the background"));
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "printf hi"}),
    ));
    let data = setup_models(cx, &[&faux], false);
    crate::tests_conversation::set_permission_mode(
        &data,
        zp_core::permissions::PermissionMode::Ask,
    );
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let background = active(window, cx);
    send(window, "work", cx);
    // Switch away while it runs.
    window
        .update(cx, |v, window, cx| v.new_tab(window, cx))
        .unwrap();
    idle(&background, 2, cx);
    let shown = cx.shown_system_notifications();
    assert_eq!(shown.len(), 1);
    assert!(shown[0].title.ends_with("— work"), "{}", shown[0].title);
    assert_eq!(shown[0].body.as_ref(), "Finished");

    // An approval in a background tab notifies too; clicking returns to the tab.
    window
        .update(cx, |v, window, cx| v.activate_tab(0, window, cx))
        .unwrap();
    send(window, "run something", cx);
    window
        .update(cx, |v, window, cx| v.activate_tab(1, window, cx))
        .unwrap();
    wait_until(cx, |cx| {
        background.read_with(cx, |v, _| v.needs_attention())
    });
    let shown = cx.shown_system_notifications();
    assert_eq!(
        shown.last().unwrap().body.as_ref(),
        "Needs approval: $ printf hi"
    );
    cx.simulate_system_notification_response(gpui::SystemNotificationResponse {
        tag: shown.last().unwrap().tag.clone(),
        action_id: None,
    });
    cx.run_until_parked();
    assert_eq!(window.update(cx, |v, _, _| v.active_index()).unwrap(), 0);
    background.update(cx, |v, cx| v.abort(cx));
}

#[cfg(unix)]
#[gpui::test]
fn mcp_tools_reach_the_agent(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "mcp__mock__echo",
        json!({"text": "hi"}),
    ));
    faux.push(FauxResponse::text("the server said hi"));
    let data = setup_models(cx, &[&faux], false);
    let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: hi"}]}}\n' "$id" ;;
  esac
done"#;
    let config = json!({"mcpServers": {"mock": {"command": "sh", "args": ["-c", script]}, "broken": {"type": "sse", "url": "http://x"}}});
    std::fs::write(zp_mcp::config::user_file(data.path()), config.to_string()).unwrap();
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    wait_until(cx, |cx| {
        conversation.read_with(cx, |v, _| {
            v.tool_names().iter().any(|t| t == "mcp__mock__echo")
        })
    });
    let hub = cx.update(|cx| crate::mcp::hub(&project, cx)).unwrap();
    assert_eq!(hub.errors().len(), 1, "the SSE server is rejected");
    assert!(
        conversation
            .read_with(cx, |v, _| v.system_prompt())
            .contains("mcp__mock__echo: Echo text (MCP server mock)")
    );

    send(window, "ask the server", cx);
    wait_until(cx, |_| faux.pending() == 0);
    wait_quiet(&conversation, cx);
    let results: Vec<String> = conversation.read_with(cx, |v, _| {
        v.agent_messages()
            .iter()
            .filter_map(zp_agent::AgentMessage::as_tool_result)
            .map(|r| r.content.iter().filter_map(UserContent::as_text).collect())
            .collect()
    });
    assert_eq!(results, ["echo: hi"]);

    // Closing the project stops its servers.
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| crate::mcp::hub(&project, cx)).is_none());
}

#[gpui::test]
fn update_checks(cx: &mut TestAppContext) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let feed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let served = feed.clone();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}/latest.json", listener.local_addr().unwrap());
    runtime.spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let body = served.lock().unwrap().clone();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    let mut settings =
        zp_core::settings::Settings::load(&zp_core::paths::settings_file(data.path())).unwrap();
    settings.update_feed = Some(url);
    settings
        .save(&zp_core::paths::settings_file(data.path()))
        .unwrap();
    cx.update(|cx| {
        crate::store::AppSettings::update(cx, |_| {}).unwrap();
    });
    let platform = zp_core::update::platform_key();
    *feed.lock().unwrap() = json!({"version": "99.0.0", "notes": "New things", "platforms": {platform.clone(): {"url": "http://x/ZP.zip", "sha256": "00"}}}).to_string();
    let state = |cx: &mut TestAppContext| cx.update(|cx| crate::updates::state(cx));
    cx.update(|cx| crate::updates::check(false, cx));
    wait_until(cx, |cx| {
        !matches!(state(cx), crate::updates::UpdateState::Checking)
    });
    assert!(
        matches!(state(cx), crate::updates::UpdateState::Available(u) if u.version == "99.0.0")
    );

    *feed.lock().unwrap() = json!({"version": zp_core::VERSION, "platforms": {}}).to_string();
    cx.update(|cx| crate::updates::check(false, cx));
    wait_until(cx, |cx| {
        !matches!(state(cx), crate::updates::UpdateState::Checking)
    });
    assert_eq!(state(cx), crate::updates::UpdateState::UpToDate);
    drop(runtime);
}

#[gpui::test]
fn terminal_panel_runs_a_shell(cx: &mut TestAppContext) {
    let _data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let visible = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| view.layout().terminal_visible)
            .unwrap()
    };
    assert!(!visible(cx));
    cx.dispatch_action(window.into(), crate::actions::ToggleTerminal);
    cx.run_until_parked();
    assert!(visible(cx));
    let terminal = window
        .update(cx, |view, _, _| view.terminal())
        .unwrap()
        .expect("a shell started");

    // The shell (PowerShell on Windows) runs in the project and answers.
    let (first, second_command): (&[u8], &[u8]) = if cfg!(windows) {
        (
            b"echo \"zp-term-$(40+2)\"; pwd\r",
            b"echo \"second-$(1+1)\"\r",
        )
    } else {
        (b"echo zp-term-$((40+2)); pwd\r", b"echo second-$((1+1))\r")
    };
    terminal.update(cx, |view, _| view.write(first.to_vec()));
    wait_until(cx, |cx| {
        terminal.read_with(cx, |view, _| view.screen_text().contains("zp-term-42"))
    });
    let screen = terminal.read_with(cx, |view, _| view.screen_text());
    let name = project.file_name().unwrap().to_string_lossy().into_owned();
    assert!(screen.contains(&name), "{screen}");

    // The label follows the program in the foreground (Unix only); a name given by the
    // user wins.
    let labels = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, cx| view.terminal_labels(cx))
            .unwrap()
    };
    let shell = labels(cx)[0].clone();
    if cfg!(unix) {
        terminal.update(cx, |view, _| view.write(b"sleep 30\r".to_vec()));
        wait_until(cx, |cx| labels(cx) == ["sleep"]);
        terminal.update(cx, |view, _| view.write(b"\x03".to_vec()));
        wait_until(cx, |cx| labels(cx) == [shell.clone()]);
    }
    window
        .update(cx, |view, window, cx| {
            view.rename_terminal(terminal.entity_id(), Some("build".into()), window, cx)
        })
        .unwrap();
    assert_eq!(labels(cx), ["build"]);
    window
        .update(cx, |view, window, cx| {
            view.rename_terminal(terminal.entity_id(), None, window, cx)
        })
        .unwrap();
    assert_eq!(labels(cx), std::slice::from_ref(&shell));

    // Maximizing hides the conversation; restoring brings it back.
    window
        .update(cx, |view, _, cx| view.toggle_terminal_maximized(cx))
        .unwrap();
    assert!(
        window
            .update(cx, |view, _, _| view.layout().terminal_maximized)
            .unwrap()
    );
    window
        .update(cx, |view, _, cx| view.toggle_terminal_maximized(cx))
        .unwrap();

    // Hiding keeps the shell; showing it again reuses it.
    cx.dispatch_action(window.into(), crate::actions::ToggleTerminal);
    cx.run_until_parked();
    assert!(!visible(cx));
    cx.dispatch_action(window.into(), crate::actions::ToggleTerminal);
    cx.run_until_parked();
    let again = window
        .update(cx, |view, _, _| view.terminal())
        .unwrap()
        .unwrap();
    assert_eq!(again.entity_id(), terminal.entity_id());

    // More terminals: each starts in the list and becomes active; the list switches.
    cx.dispatch_action(window.into(), crate::actions::NewTerminal);
    cx.run_until_parked();
    let (tabs, active) = window
        .update(cx, |view, _, _| view.terminal_tabs())
        .unwrap();
    assert_eq!((tabs.len(), active), (2, 1));
    let second = tabs[1].clone();
    assert_ne!(second.entity_id(), terminal.entity_id());
    second.update(cx, |view, _| view.write(second_command.to_vec()));
    wait_until(cx, |cx| {
        second.read_with(cx, |view, _| view.screen_text().contains("second-2"))
    });
    assert!(!terminal.read_with(cx, |view, _| view.screen_text().contains("second-2")));

    // A shell that exits closes its terminal; the panel hides with the last one.
    second.update(cx, |view, _| view.write(b"exit\r".to_vec()));
    wait_until(cx, |cx| {
        window
            .update(cx, |view, _, _| view.terminal_tabs().0.len() == 1)
            .unwrap()
    });
    let active = window
        .update(cx, |view, _, _| view.terminal())
        .unwrap()
        .unwrap();
    assert_eq!(active.entity_id(), terminal.entity_id());
    window
        .update(cx, |view, window, cx| {
            view.close_terminal(terminal.entity_id(), window, cx)
        })
        .unwrap();
    assert!(!visible(cx));
    assert!(
        window
            .update(cx, |view, _, _| view.terminal())
            .unwrap()
            .is_none()
    );
}
