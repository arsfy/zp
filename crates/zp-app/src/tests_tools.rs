//! The `ask` and `todo` tools in the app: question cards, answers sent back to the
//! agent, and the task list above the composer.

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use serde_json::json;
use zp_ai::{FauxProvider, FauxResponse, UserContent};
use zp_core::permissions::PermissionMode;

use crate::conversation::ConversationView;
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
    conversation.read_with(cx, |view, _| {
        assert_eq!(view.pending_approvals().count(), 0);
        assert!(view.needs_attention());
        let questions = &view.pending_questions()[0].1;
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].options[1].description, "One per level");
        // The task list is shown while the agent works.
        let todos = view.current_todos().unwrap();
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
