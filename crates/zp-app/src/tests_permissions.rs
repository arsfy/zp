//! M8: approval cards, session grants, the permission mode picker, stopping while waiting
//! and project trust — with real tools in a temp project.

use gpui::{Entity, IntoElement as _, Render as _, TestAppContext};
use serde_json::json;
use zp_ai::{FauxProvider, FauxResponse};
use zp_core::permissions::{Decision, PermissionMode};

use crate::actions::StopRun;
use crate::conversation::{ConversationView, Picker};
use crate::tests::{project_dir, saved_state};
use crate::tests_conversation::{
    active, open, send, set_permission_mode, setup_models, wait_until,
};

fn pending(conversation: &Entity<ConversationView>, cx: &mut TestAppContext) -> Vec<String> {
    conversation.read_with(cx, |view, _| {
        view.pending_approvals()
            .map(|request| request.summary.clone())
            .collect()
    })
}

fn wait_for_approval(conversation: &Entity<ConversationView>, cx: &mut TestAppContext) -> String {
    wait_until(cx, |cx| !pending(conversation, cx).is_empty());
    conversation.read_with(cx, |view, _| {
        view.pending_approvals()
            .next()
            .unwrap()
            .tool_call_id
            .clone()
    })
}

fn tool_results(
    conversation: &Entity<ConversationView>,
    cx: &mut TestAppContext,
) -> Vec<(bool, String)> {
    conversation.read_with(cx, |view, _| {
        view.agent_messages()
            .iter()
            .filter_map(zp_agent::AgentMessage::as_tool_result)
            .map(|r| {
                (
                    r.is_error,
                    r.content
                        .iter()
                        .filter_map(zp_ai::UserContent::as_text)
                        .collect(),
                )
            })
            .collect()
    })
}

#[gpui::test]
fn approvals_with_reasons_grants_and_previews(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "echo hi > out.txt"}),
    ));
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "echo one"}),
    ));
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "echo two"}),
    ));
    faux.push(FauxResponse::tool_call(
        "edit",
        json!({"path": "a.txt", "edits": [{"oldText": "old", "newText": "new"}]}),
    ));
    faux.push(FauxResponse::text("done"));
    let data = setup_models(cx, &[&faux], false);
    set_permission_mode(&data, PermissionMode::Ask);
    let (_dir, project) = project_dir();
    std::fs::write(project.join("a.txt"), "old\n").unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    assert_eq!(
        conversation.read_with(cx, |view, _| view.permission_mode()),
        PermissionMode::Ask
    );
    send(window, "go", cx);

    // The first command waits; the tab shows it.
    let first = wait_for_approval(&conversation, cx);
    assert_eq!(pending(&conversation, cx), ["$ echo hi > out.txt"]);
    let attention = window
        .update(cx, |view, _, cx| {
            view.tabs()[0].conversation.read(cx).needs_attention()
        })
        .unwrap();
    assert!(attention);
    // Deny with a reason typed in the card.
    let reason = conversation
        .read_with(cx, |view, _| view.approval_reason(&first))
        .unwrap();
    reason.update(cx, |input, cx| {
        input.emplace("write to target/ instead", cx)
    });
    conversation.update(cx, |view, cx| view.deny_approval(&first, cx));

    // Approving `echo` for the session lets the next `echo` run on its own.
    let second = wait_for_approval(&conversation, cx);
    assert_eq!(pending(&conversation, cx), ["$ echo one"]);
    conversation.update(cx, |view, cx| {
        view.answer_approval(&second, Decision::AllowForSession, cx)
    });

    // The edit asks too, with a preview of the change.
    let edit = wait_for_approval(&conversation, cx);
    assert_eq!(pending(&conversation, cx), ["a.txt"]);
    assert_eq!(
        conversation.read_with(cx, |view, _| view.approval_preview(&edit)),
        Some((1, 1))
    );
    conversation.update(cx, |view, cx| {
        view.answer_approval(&edit, Decision::Allow, cx)
    });
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| !view.is_running())
    });

    assert_eq!(
        tool_results(&conversation, cx),
        [
            (
                true,
                "The user denied this tool call. Their reason: write to target/ instead".to_owned()
            ),
            (false, "one".to_owned()),
            (false, "two".to_owned()),
            (
                false,
                "Successfully replaced 1 block(s) in a.txt.".to_owned()
            ),
        ]
    );
    assert!(!project.join("out.txt").exists());
    assert_eq!(
        std::fs::read_to_string(project.join("a.txt")).unwrap(),
        "new\n"
    );
    assert!(pending(&conversation, cx).is_empty());
}

#[gpui::test]
fn permission_modes_render_with_picker_open_and_closed(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    for mode in PermissionMode::ALL {
        window
            .update(cx, |_, window, cx| {
                conversation.update(cx, |view, cx| {
                    view.select_permission_mode(mode, window, cx);
                    let _ = view.render(window, cx).into_any_element();
                    view.toggle_picker(Picker::Permissions, window, cx);
                    let _ = view.render(window, cx).into_any_element();
                });
            })
            .unwrap();
        cx.run_until_parked();
    }
}

#[gpui::test]
fn permission_mode_is_per_project(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    set_permission_mode(&data, PermissionMode::Ask);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let first = active(window, cx);
    window
        .update(cx, |view, window, cx| view.new_tab(window, cx))
        .unwrap();
    let second = active(window, cx);
    window
        .update(cx, |_, window, cx| {
            second.update(cx, |view, cx| {
                view.select_permission_mode(PermissionMode::AutoEdit, window, cx)
            })
        })
        .unwrap();
    cx.run_until_parked();
    // Every tab of the project follows, and the choice is remembered.
    assert_eq!(
        first.read_with(cx, |v, _| v.permission_mode()),
        PermissionMode::AutoEdit
    );
    assert_eq!(
        saved_state(cx, &data).project(&project).permission_mode,
        Some(PermissionMode::AutoEdit)
    );
    window
        .update(cx, |view, window, cx| view.new_tab(window, cx))
        .unwrap();
    assert_eq!(
        active(window, cx).read_with(cx, |v, _| v.permission_mode()),
        PermissionMode::AutoEdit
    );
}

#[gpui::test]
fn stopping_while_waiting_for_approval(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::tool_call(
        "bash",
        json!({"command": "rm -rf build"}),
    ));
    let data = setup_models(cx, &[&faux], false);
    set_permission_mode(&data, PermissionMode::AutoReview);
    let (_dir, project) = project_dir();
    std::fs::create_dir(project.join("build")).unwrap();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    send(window, "clean", cx);
    // Destructive: asks the user even in auto-review (no review request is made).
    wait_for_approval(&conversation, cx);
    let reasons = conversation.read_with(cx, |view, _| {
        view.pending_approvals().next().unwrap().reasons.clone()
    });
    assert_eq!(
        reasons,
        [zp_core::permissions::AskReason::Dangerous(
            "deletes files (`rm`)".into()
        )]
    );
    assert_eq!(faux.requests().len(), 1);

    cx.dispatch_action(window.into(), StopRun);
    wait_until(cx, |cx| {
        conversation.read_with(cx, |view, _| !view.is_running())
    });
    assert!(pending(&conversation, cx).is_empty());
    assert!(project.join("build").exists());
}

#[gpui::test]
fn trusting_a_project(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    set_permission_mode(&data, PermissionMode::Ask);
    let (_dir, project) = project_dir();
    std::fs::write(project.join("AGENTS.md"), "Indent with tabs.").unwrap();
    std::fs::create_dir(project.join(".zp")).unwrap();
    std::fs::write(
        project.join(".zp/settings.json"),
        r#"{"permissions": {"mode": "fullAuto"}}"#,
    )
    .unwrap();

    let window = open(cx, &project);
    assert!(cx.has_pending_prompt(), "asked on first open");
    cx.simulate_prompt_answer("Don't Trust");
    cx.run_until_parked();
    let conversation = active(window, cx);
    let state = |cx: &mut TestAppContext| {
        conversation.read_with(cx, |view, _| {
            (
                view.permission_mode(),
                view.system_prompt().contains("Indent with tabs."),
            )
        })
    };
    assert_eq!(state(cx), (PermissionMode::Ask, false));
    assert_eq!(
        saved_state(cx, &data).project(&project).trusted,
        Some(false)
    );

    // Not asked again; trusting later (from the status bar) applies right away.
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let window = open(cx, &project);
    assert!(!cx.has_pending_prompt());
    let conversation = active(window, cx);
    window
        .update(cx, |view, window, cx| view.prompt_trust(window, cx))
        .unwrap();
    cx.simulate_prompt_answer("Trust Project");
    cx.run_until_parked();
    let state = conversation.read_with(cx, |view, _| {
        (
            view.permission_mode(),
            view.system_prompt().contains("Indent with tabs."),
        )
    });
    assert_eq!(state, (PermissionMode::FullAuto, true));
}
