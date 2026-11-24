//! Tool cards in the app: file previews, question cards, answers sent back to the
//! agent, and the task list above the composer.

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use serde_json::json;
use zp_ai::{FauxBlock, FauxProvider, FauxResponse, UserContent};
use zp_core::permissions::PermissionMode;

use crate::conversation::ConversationView;
use crate::conversation::transcript::ToolStatus;
use crate::project::ProjectWindow;
use crate::tests::project_dir;
use crate::tests_conversation::{
    active, idle, open, send, set_permission_mode, setup_models, wait_until,
};

/// Text of the agent's tool results, in order.
fn tool_results(conversation: &Entity<ConversationView>, cx: &mut TestAppContext) -> Vec<String> {
    conversation.read_with(cx, |view, _| {
        view.agent_messages()
            .iter()
            .filter_map(zp_agent::AgentMessage::as_tool_result)
            .map(|result| {
                result
                    .content
                    .iter()
                    .filter_map(UserContent::as_text)
                    .collect()
            })
            .collect()
    })
}

/// Waits for an `ask` card and returns its call id.
fn wait_for_questions(conversation: &Entity<ConversationView>, cx: &mut TestAppContext) -> String {
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| !view.pending_questions().is_empty())
    });
    conversation.read_with(cx, |view, _| view.pending_questions()[0].0.clone())
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
fn file_tools_show_read_code_and_edit_content_while_it_streams(cx: &mut TestAppContext) {
    let faux = FauxProvider::default().with_chunk_delay(std::time::Duration::from_millis(200));
    let before = "fn version() -> u32 { 1 }\n";
    let after = "fn version() -> u32 { 2 }\n";
    faux.push(FauxResponse::tool_call("read", json!({"path": "demo.rs"})));
    faux.push(FauxResponse::tool_call(
        "edit",
        json!({"path": "demo.rs", "edits": [{"oldText": before, "newText": after}]}),
    ));
    faux.push(FauxResponse::text("Updated the file."));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let file = project.join("demo.rs");
    std::fs::write(&file, before).unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "Read the source and bump the version", cx);

    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| {
            view.transcript()
                .tool("faux_call_0")
                .is_some_and(|tool| tool.status == ToolStatus::Done)
        })
    });
    conversation.update(cx, |view, cx| view.toggle_tool("faux_call_0", 1, false, cx));
    wait_until(cx, |cx| {
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("tool-read-output")
            .is_some()
    });
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| view.transcript().is_streaming(2))
            && VisualTestContext::from_window(window.into(), cx)
                .debug_bounds("tool-live-file-content")
                .is_some()
    });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    idle(&conversation, 4, cx);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), after);
}

#[gpui::test]
fn todo_and_ask_run_without_approval_and_answers_reach_the_agent(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "todo",
        json!({"todos": [
            {"content": "Pick a style", "status": "in_progress"},
            {"content": "Format the code", "status": "pending"}
        ]}),
    ));
    faux.push(FauxResponse::tool_call(
        "ask",
        json!({"questions": [
            {"question": "Which indent?", "options": [{"label": "Spaces"}, {"label": "Tabs", "description": "One per level"}]},
            {"question": "Anything else?"}
        ]}),
    ));
    faux.push(FauxResponse::text("Formatting with tabs."));
    let data = setup_models(cx, &[&faux], false);
    // Even the strictest mode does not ask about these tools.
    set_permission_mode(&data, PermissionMode::Ask);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "format it", cx);

    let call = wait_for_questions(&conversation, cx);
    // Questions arrive on their own channel, the todo call's result with the batched agent
    // events, which can lag behind on a busy machine.
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| view.current_todos().is_some())
    });
    conversation.read_with(cx, |view, _| {
        assert_eq!(view.pending_approvals().count(), 0);
        assert!(view.needs_attention());
        let questions = &view.pending_questions()[0].1;
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].options[1].description, "One per level");
        // The task list is shown while the agent works.
        let (_, todos) = view.current_todos().unwrap();
        assert_eq!((todos.completed(), todos.todos.len()), (0, 2));
        assert_eq!(todos.current().unwrap().content, "Pick a style");
    });
    assert!(
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("todo-panel")
            .is_some()
    );

    // With two questions, picking an option does not send yet.
    click(window, "question-option-0-1", cx);
    assert_eq!(
        conversation.read_with(cx, |view, _| view.pending_questions().len()),
        1
    );
    let text = conversation
        .read_with(cx, |view, _| view.question_input(&call, 1))
        .unwrap();
    text.update(cx, |input, cx| input.emplace("Keep line endings", cx));
    click(window, "question-submit", cx);
    idle(&conversation, 4, cx);

    let results = tool_results(&conversation, cx);
    assert!(results[0].starts_with("Task list updated (0/2 done):\n[>] Pick a style"));
    assert_eq!(
        results[1],
        "Q: Which indent?\nA: Tabs\n\nQ: Anything else?\nA: Keep line endings"
    );
    assert!(!conversation.read_with(cx, |view, _| view.needs_attention()));
}

#[gpui::test]
fn completed_tasks_stay_until_hidden_and_new_tasks_reappear(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "todo",
        json!({"todos": [{"content": "Finish the first task", "status": "completed"}]}),
    ));
    faux.push(FauxResponse::text("Done"));
    faux.push(FauxResponse::text("Still done"));
    faux.push(FauxResponse::tool_call(
        "todo",
        json!({"todos": [{"content": "Start the next task", "status": "in_progress"}]}),
    ));
    faux.push(FauxResponse::text("Started"));
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    let visible = |selector, cx: &mut TestAppContext| {
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds(selector)
            .is_some()
    };

    send(window, "Finish the first task", cx);
    idle(&conversation, 3, cx);
    assert!(visible("todo-panel", cx));
    click(window, "hide-completed-tasks", cx);
    cx.run_until_parked();
    assert!(!visible("todo-panel", cx));

    send(window, "Confirm the first task is done", cx);
    idle(&conversation, 5, cx);
    assert!(!visible("todo-panel", cx));

    send(window, "Start the next task", cx);
    idle(&conversation, 8, cx);
    assert!(visible("todo-panel", cx));
    assert!(!visible("hide-completed-tasks", cx));
}

#[gpui::test]
fn single_choice_answers_on_click_and_skip_dismisses(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let question = json!({"questions": [
        {"question": "Ship it?", "options": [{"label": "Yes"}, {"label": "No"}]}
    ]});
    faux.push(FauxResponse::tool_call("ask", question.clone()));
    faux.push(FauxResponse::tool_call("ask", question));
    faux.push(FauxResponse::text("ok"));
    setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "release", cx);

    wait_for_questions(&conversation, cx);
    click(window, "question-option-0-0", cx);
    wait_for_questions(&conversation, cx);
    click(window, "question-dismiss", cx);
    idle(&conversation, 4, cx);

    let results = tool_results(&conversation, cx);
    assert_eq!(results[0], "Q: Ship it?\nA: Yes");
    assert!(results[1].starts_with("The user dismissed the questions"));
}

#[gpui::test]
fn a_subagent_card_stays_open_while_it_runs_and_a_cancelled_one_does_not_count_as_failed(
    cx: &mut TestAppContext,
) {
    use zp_core::subagents::{Approval, Report, Status, SubagentSettings};

    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "subagent",
        json!({"task": "Find where add() is tested"}),
    ));
    faux.push(FauxResponse::blocks(vec![
        FauxBlock::Text("Looking for tests of add()".into()),
        FauxBlock::WaitForCancellation,
    ]));
    faux.push(FauxResponse::text("Done"));
    let _data = setup_models(cx, &[&faux], false);
    cx.update(|cx| {
        crate::store::AppSettings::update(cx, |settings| {
            settings.subagents = Some(SubagentSettings {
                enabled: Some(true),
                approval: Some(Approval::Allowlisted),
                ..SubagentSettings::default()
            });
        })
        .unwrap()
    });
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "Delegate it", cx);
    let report = |cx: &mut TestAppContext| {
        conversation.read_with(cx, |view, _| {
            let call = view.transcript().assistant(1)?.tool_calls().next()?.clone();
            let details = view.transcript().tool(&call.id)?.details.clone()?;
            Some((call, Report::from_details(&details)?))
        })
    };
    wait_until(cx, |cx| {
        report(cx).is_some_and(|(_, report)| report.status == Status::Running)
    });
    let (call, _) = report(cx).unwrap();
    assert!(
        conversation.update(cx, |view, cx| view.tool_is_expanded(1, &call, false, cx)),
        "open while running, though other tools are collapsed"
    );
    // What the child is writing shows in the card as it streams.
    wait_until(cx, |cx| {
        report(cx).is_some_and(|(_, report)| report.activity.is_some())
            && VisualTestContext::from_window(window.into(), cx)
                .debug_bounds("subagent-activity")
                .is_some()
    });

    click(window, "subagent-cancel", cx);
    idle(&conversation, 3, cx);
    assert_eq!(report(cx).unwrap().1.status, Status::Cancelled);
    cx.executor()
        .advance_clock(crate::conversation::MIN_EXPANDED);
    assert!(
        !conversation.update(cx, |view, cx| view.tool_is_expanded(1, &call, false, cx)),
        "a cancelled task is not opened like a failure"
    );
}

#[gpui::test]
fn temporary_files_a_run_created_can_be_measured_and_deleted(cx: &mut TestAppContext) {
    let (_dir, project) = project_dir();
    // A temporary directory other than the one holding the test project.
    let Some(root) = zp_core::temp::scratch_roots(&project).into_iter().next() else {
        return;
    };
    let name = format!(
        "zp-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let made = root.path.join(&name);
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": format!("mkdir -p '{0}' && printf hello > '{0}/out.txt'", made.display())}),
    ));
    faux.push(FauxResponse::text("Done"));
    let _data = setup_models(cx, &[&faux], false);
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "Use a scratch directory", cx);
    idle(&conversation, 3, cx);
    cx.run_until_parked();

    assert_eq!(
        conversation.read_with(cx, |view, _| view.temp_paths(2)),
        Some(vec![made.clone()]),
        "listed under the run's last row"
    );
    assert!(
        VisualTestContext::from_window(window.into(), cx)
            .debug_bounds("temp-files")
            .is_some()
    );
    click(window, "temp-files-size", cx);
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| view.temp_size(2)) == Some(5)
    });
    click(window, "temp-files-clean", cx);
    wait_until(cx, |_| !made.exists());
    assert!(root.path.exists(), "only what the run created");
}
