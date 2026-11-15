//! The M3 flow — read, edit, run tests, reopen and continue — with the faux provider and
//! the real tools in a temp project.

use std::sync::{Arc, Mutex};

use serde_json::json;
use zp_agent::{AgentEvent, AgentMessage};
use zp_ai::{FauxBlock, FauxProvider, FauxResponse, Message, Models, ThinkingLevel, UserContent};
use zp_core::{CodingSession, CodingSessionOptions, session_model};
use zp_session::{EntryData, SessionManager};
use zp_tools::FileChange;

fn models(faux: &FauxProvider) -> Models {
    let mut models = Models::new();
    models.set_provider(Arc::new(faux.clone()));
    models
}

fn call(name: &str, arguments: serde_json::Value) -> FauxResponse {
    FauxResponse::blocks(vec![FauxBlock::ToolCall {
        name: name.into(),
        arguments,
    }])
}

fn text(content: &[UserContent]) -> String {
    content.iter().filter_map(UserContent::as_text).collect()
}

#[tokio::test]
async fn read_edit_test_then_resume() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let cwd = project.path().to_path_buf();
    std::fs::write(cwd.join("add.sh"), "echo $((2+2))\n").unwrap();
    std::fs::write(
        cwd.join("test.sh"),
        "echo running\nsleep 0.3\nif [ \"$(sh add.sh)\" = 5 ]; then echo PASS; else echo FAIL; exit 1; fi\n",
    )
    .unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "Always run test.sh after edits.").unwrap();

    let faux = FauxProvider::default();
    faux.push(call("read", json!({"path": "add.sh"})));
    faux.push(call("bash", json!({"command": "sh test.sh"})));
    faux.push(call(
        "edit",
        json!({"path": "add.sh", "edits": [{"oldText": "2+2", "newText": "2+3"}]}),
    ));
    faux.push(call("bash", json!({"command": "sh test.sh"})));
    faux.push(FauxResponse::text("Fixed: add.sh now prints 5."));

    let manager = SessionManager::new(data.path().join("sessions"));
    let mut options = CodingSessionOptions::new(models(&faux), &cwd, manager.create(&cwd));
    options.model = Some(faux.model());
    options.thinking_level = Some(ThinkingLevel::Low);
    let coding = CodingSession::new(options).unwrap();
    let session_file = coding.session().file().unwrap().to_path_buf();

    let updates = Arc::new(Mutex::new(Vec::new()));
    let sink = updates.clone();
    let _subscription = coding.agent().subscribe(move |event| {
        if let AgentEvent::ToolExecutionUpdate { partial_result, .. } = event {
            sink.lock().unwrap().push(text(&partial_result.content));
        }
    });

    let added = coding
        .prompt("The test fails, please fix add.sh")
        .await
        .unwrap();
    let results: Vec<_> = added
        .iter()
        .filter_map(AgentMessage::as_tool_result)
        .collect();
    assert_eq!(results.len(), 4);
    assert_eq!(text(&results[0].content), "echo $((2+2))\n");
    assert!(results[1].is_error);
    assert!(text(&results[1].content).contains("FAIL"));
    let change = FileChange::from_details(results[2].details.as_ref().unwrap()).unwrap();
    assert_eq!(change.after, "echo $((2+3))\n");
    assert!(!results[3].is_error);
    assert!(text(&results[3].content).ends_with("PASS"));
    assert_eq!(
        std::fs::read_to_string(cwd.join("add.sh")).unwrap(),
        "echo $((2+3))\n"
    );
    assert!(
        updates
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.contains("running")),
        "bash streams progress"
    );

    // The request carried the system prompt with project context and every tool.
    let request = &faux.requests()[0];
    let system = request.system_prompt.as_deref().unwrap();
    assert!(system.contains("Always run test.sh after edits."));
    assert!(system.contains(&format!(
        "<cwd>\n{}\n</cwd>",
        cwd.to_string_lossy().replace('\\', "/")
    )));
    assert_eq!(request.tools.len(), 7);

    // Everything was persisted as it happened.
    let reopened = manager.open(&session_file).unwrap();
    let kinds: Vec<&str> = reopened.entries().iter().map(|e| e.kind()).collect();
    assert_eq!(kinds[..2], ["model_change", "thinking_level_change"]);
    assert_eq!(reopened.context().messages.len(), 10);
    assert_eq!(coding.usage().output, 25);
    drop(coding);

    // Resume: the model and transcript come back and the conversation continues.
    faux.push(FauxResponse::text("You're welcome."));
    let models = models(&faux);
    assert_eq!(session_model(&models, &reopened).unwrap().id, "faux-1");
    let resumed = CodingSession::new(CodingSessionOptions::new(models, &cwd, reopened)).unwrap();
    assert_eq!(resumed.agent().thinking_level(), ThinkingLevel::Low);
    assert_eq!(resumed.agent().messages().len(), 10);
    resumed.prompt("thanks").await.unwrap();

    let last_request = faux.requests().last().unwrap().clone();
    assert_eq!(last_request.messages.len(), 11);
    let Message::User(user) = last_request.messages.last().unwrap() else {
        panic!("expected the new prompt last")
    };
    assert_eq!(text(&user.content), "thanks");
    let entries = manager.open(&session_file).unwrap();
    assert_eq!(entries.context().messages.len(), 12);
    // No duplicate model/thinking records on resume.
    let changes = entries
        .entries()
        .iter()
        .filter(|e| {
            matches!(
                e.data,
                EntryData::ModelChange { .. } | EntryData::ThinkingLevelChange { .. }
            )
        })
        .count();
    assert_eq!(changes, 2);
}

#[tokio::test]
async fn retry_after_a_failed_response() {
    let project = tempfile::tempdir().unwrap();
    let faux = FauxProvider::default();
    faux.push(FauxResponse::Error("overloaded".into()));
    faux.push(FauxResponse::text("second try"));
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(faux.model());
    let coding = CodingSession::new(options).unwrap();

    assert!(coding.prepare_retry().is_err(), "nothing failed yet");
    coding.prompt("hi").await.unwrap();
    assert_eq!(coding.agent().messages().len(), 2);
    coding.prepare_retry().unwrap();
    assert_eq!(coding.agent().messages().len(), 1);
    let added = coding.agent().continue_run().await.unwrap();
    assert_eq!(added[0].as_assistant().unwrap().text(), "second try");
    // The request did not include the failed response.
    assert_eq!(faux.requests()[1].messages.len(), 1);
    let session = coding.session();
    assert_eq!(session.context().messages.len(), 2);
    assert_eq!(
        session
            .entries()
            .iter()
            .filter(|e| e.message().is_some())
            .count(),
        3,
        "the failed attempt is kept on its own branch"
    );
}

#[tokio::test]
async fn branching_rewinds_the_agent_transcript() {
    let project = tempfile::tempdir().unwrap();
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("first answer"));
    faux.push(FauxResponse::text("second answer"));
    faux.push(FauxResponse::text("alternative"));
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(faux.model());
    let coding = CodingSession::new(options).unwrap();

    coding.prompt("one").await.unwrap();
    let first_answer = coding.session().leaf_id().unwrap().to_owned();
    coding.prompt("two").await.unwrap();
    assert_eq!(coding.agent().messages().len(), 4);

    coding.branch(&first_answer).unwrap();
    assert_eq!(coding.agent().messages().len(), 2);
    coding.prompt("two, differently").await.unwrap();
    assert_eq!(faux.requests().last().unwrap().messages.len(), 3);
    assert_eq!(coding.session().context().messages.len(), 4);
    assert_eq!(coding.session().children(&first_answer).len(), 2);

    coding.set_name(Some("Experiment".into())).unwrap();
    assert_eq!(coding.session().name(), Some("Experiment"));
}

#[tokio::test]
async fn titles_and_branch_navigation() {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("\"Fix the flaky test.\""));
    let models = models(&faux);
    let title = zp_core::title::generate_title(&models, &faux.model(), "the test is flaky")
        .await
        .unwrap();
    assert_eq!(title, "Fix the flaky test");
    assert_eq!(faux.requests()[0].messages.len(), 1);
    // Faux models are free, so the current model is used.
    assert_eq!(
        zp_core::title::title_model(&models, &faux.model()).id,
        "faux-1"
    );

    let project = tempfile::tempdir().unwrap();
    faux.push(FauxResponse::text("a1"));
    faux.push(FauxResponse::text("a2"));
    faux.push(FauxResponse::text("a2 again"));
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models, project.path(), session);
    options.model = Some(faux.model());
    let coding = CodingSession::new(options).unwrap();
    coding.prompt("u1").await.unwrap();
    coding.prompt("u2").await.unwrap();
    let ids = coding.context_entry_ids();
    assert_eq!(ids.len(), 4);

    // Edit & resend the second user message: a sibling branch.
    coding.branch_before(&ids[2]).unwrap();
    assert_eq!(coding.agent().messages().len(), 2);
    coding.prompt("u2 edited").await.unwrap();
    assert_eq!(coding.session().children(&ids[1]).len(), 2);

    // Switching back to the original branch lands on its latest message.
    coding.switch_branch(&ids[2]).unwrap();
    let messages = coding.agent().messages();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[3].as_assistant().unwrap().text(), "a2");
}
