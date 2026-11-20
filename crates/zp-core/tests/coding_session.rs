//! The M3 flow — read, edit, run tests, reopen and continue — with the faux provider and
//! the real tools in a temp project.

use std::sync::{Arc, Mutex};

use serde_json::json;
use zp_agent::{AgentEvent, AgentMessage};
use zp_ai::{FauxBlock, FauxProvider, FauxResponse, Message, Models, ThinkingLevel, UserContent};
use zp_core::changes::{GroupKind, Scope};
use zp_core::checkpoint::{BlobStore, RecordedChange};
use zp_core::restore::RestorePlan;
use zp_core::{CodingSession, CodingSessionOptions, session_model};
use zp_session::{EntryData, SessionManager};

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
    let blobs = BlobStore::in_data_dir(data.path());
    let mut options = CodingSessionOptions::new(models(&faux), &cwd, manager.create(&cwd));
    options.model = Some(faux.model());
    options.thinking_level = Some(ThinkingLevel::Low);
    options.blobs = Some(blobs.clone());
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
    // The edit's versions went to the snapshot store; the session keeps hashes.
    let details = results[2].details.as_ref().unwrap();
    assert!(details.get("after").is_none());
    let change = RecordedChange::from_details(details).unwrap();
    assert_eq!((change.added, change.removed), (1, 1));
    assert_eq!(
        &*change.after.load(&blobs).unwrap().unwrap(),
        "echo $((2+3))\n"
    );
    assert!(data.path().join("checkpoints/objects").is_dir());
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
    assert_eq!(request.tools.len(), zp_tools::ALL_TOOLS.len());

    // Everything was persisted as it happened.
    let reopened = manager.open(&session_file).unwrap();
    let kinds: Vec<&str> = reopened.entries().iter().map(|e| e.kind()).collect();
    assert_eq!(kinds[..2], ["model_change", "thinking_level_change"]);
    assert_eq!(reopened.context().messages.len(), 10);
    assert_eq!(coding.usage().output, 25);

    // The change log has the turn; reverting it restores the file and tells the model.
    let log = coding.change_log();
    assert_eq!(log.groups.len(), 1);
    let files = log.files(Scope::Session);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, cwd.join("add.sh"));
    let plan = RestorePlan::prepare(&log.revert_targets(Scope::Group(0), None), &blobs).unwrap();
    assert_eq!(plan.conflicts().count(), 0);
    let restored = plan.apply(false, &blobs).unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("add.sh")).unwrap(),
        "echo $((2+2))\n"
    );
    coding.record_restore("Reverted turn 1", &restored).unwrap();
    let log = coding.change_log();
    assert!(
        matches!(&log.groups[1].kind, GroupKind::Restore { description } if description == "Reverted turn 1")
    );
    assert!(log.files(Scope::Session)[0].is_unchanged());
    assert_eq!(coding.agent().messages().len(), 11);
    // Put the fix back for the rest of the test.
    let plan = RestorePlan::prepare(&log.revert_targets(Scope::Group(1), None), &blobs).unwrap();
    let redone = plan.apply(false, &blobs).unwrap();
    coding
        .record_restore("Reverted the restore", &redone)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("add.sh")).unwrap(),
        "echo $((2+3))\n"
    );
    drop(coding);
    // Two restore records were added.
    let reopened = manager.open(&session_file).unwrap();
    assert_eq!(reopened.context().messages.len(), 12);

    // Resume: the model and transcript come back and the conversation continues.
    faux.push(FauxResponse::text("You're welcome."));
    let models = models(&faux);
    assert_eq!(session_model(&models, &reopened).unwrap().id, "faux-1");
    let resumed = CodingSession::new(CodingSessionOptions::new(models, &cwd, reopened)).unwrap();
    assert_eq!(resumed.agent().thinking_level(), ThinkingLevel::Low);
    assert_eq!(resumed.agent().messages().len(), 12);
    resumed.prompt("thanks").await.unwrap();

    let last_request = faux.requests().last().unwrap().clone();
    assert_eq!(last_request.messages.len(), 13);
    let Message::User(user) = last_request.messages.last().unwrap() else {
        panic!("expected the new prompt last")
    };
    assert_eq!(text(&user.content), "thanks");
    let entries = manager.open(&session_file).unwrap();
    assert_eq!(entries.context().messages.len(), 14);
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
async fn default_thinking_and_recorded_override() {
    let project = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let manager = SessionManager::new(data.path().join("sessions"));
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("hello"));
    let session = manager.create(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(faux.model());
    let coding = CodingSession::new(options).unwrap();
    assert_eq!(coding.agent().thinking_level(), ThinkingLevel::High);
    assert_eq!(
        coding.session().context().thinking_level,
        Some(ThinkingLevel::High)
    );
    coding.prompt("hi").await.unwrap();

    coding.set_thinking_level(ThinkingLevel::Off).unwrap();
    let session_file = coding.session().file().unwrap().to_path_buf();
    drop(coding);
    let session = manager.open(&session_file).unwrap();
    let options = CodingSessionOptions::new(models(&faux), project.path(), session);
    let resumed = CodingSession::new(options).unwrap();
    assert_eq!(resumed.agent().thinking_level(), ThinkingLevel::Off);
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

#[tokio::test]
async fn a_session_can_start_without_a_model() {
    let project = tempfile::tempdir().unwrap();
    let cwd = project.path().to_path_buf();
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("hi"));
    let session = zp_session::Session::in_memory(&cwd);
    let coding = CodingSession::new(CodingSessionOptions::new(models(&faux), &cwd, session))
        .expect("no model is needed to open a session");

    // No model is chosen or recorded on the user's behalf; runs fail until one is set.
    assert!(coding.agent().model().is_none());
    assert!(coding.session().context().model.is_none());
    assert!(!coding.needs_compaction());
    assert!(matches!(
        coding.prompt("hello").await,
        Err(zp_agent::AgentError::NoModel)
    ));
    assert!(coding.agent().messages().is_empty());
    assert!(faux.requests().is_empty());

    coding.set_model(faux.model()).unwrap();
    assert_eq!(coding.session().context().model.unwrap().model_id, "faux-1");
    coding.prompt("hello").await.unwrap();
    assert_eq!(faux.requests().len(), 1);
}

#[tokio::test]
async fn compacts_before_the_model_request_after_tool_results() {
    let project = tempfile::tempdir().unwrap();
    // This result crosses the threshold for a continuation that would otherwise fit.
    std::fs::write(project.path().join("large.txt"), "x".repeat(1600)).unwrap();
    let faux = FauxProvider::default();
    for response in [
        FauxResponse::text("old answer"),
        call("read", json!({"path": "large.txt"})),
        FauxResponse::text("old summary"),
        FauxResponse::text("turn summary"),
        FauxResponse::text("final answer"),
    ] {
        faux.push(response);
    }
    let model = faux.model();
    let reserve_tokens = model.context_window - 340;
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(model);
    options.custom_system_prompt = Some("short system prompt".into());
    options.compaction.reserve_tokens = reserve_tokens;
    options.compaction.keep_recent_tokens = 100;
    let coding = CodingSession::new(options).unwrap();
    coding.prompt("old question").await.unwrap();
    coding.prompt("read the file").await.unwrap();

    assert_eq!(faux.pending(), 0);
    let requests = faux.requests();
    assert!(matches!(
        &requests[1].messages.last(),
        Some(Message::User(_))
    ));
    assert!(requests[2].tools.is_empty());
    assert!(requests[3].tools.is_empty());
    assert!(matches!(
        &requests[4].messages[..],
        [
            Message::User(_),
            Message::Assistant(_),
            Message::ToolResult(_)
        ]
    ));
    assert_eq!(
        coding.agent().messages(),
        coding.session().context().messages
    );
    assert_eq!(
        coding.usage().output,
        25,
        "includes summarized responses and both summary requests"
    );
}

#[tokio::test]
async fn compaction_invalidates_retained_usage_and_failed_compaction_preserves_the_leaf() {
    let project = tempfile::tempdir().unwrap();
    let faux = FauxProvider::default();
    let model = faux.model();
    let mut session = zp_session::Session::in_memory(project.path());
    session.append_message("old question".into()).unwrap();
    let kept = session.append_message("recent question".into()).unwrap();
    let mut reply = zp_ai::AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);
    reply.usage.input = 125_000;
    reply.usage.update_total();
    session.append_message(reply.into()).unwrap();
    session
        .append_compaction("short summary".into(), &kept, 125_000, None)
        .unwrap();
    session.append_message("new question".into()).unwrap();
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(model);
    let coding = CodingSession::new(options).unwrap();
    assert!(coding.context_tokens() < 10_000);
    assert!(!coding.needs_compaction());
    assert_eq!(coding.usage().input, 125_000);

    faux.push(FauxResponse::Error("context_length_exceeded".into()));
    faux.push(FauxResponse::Error("auto summary failed".into()));
    coding.prompt("overflow").await.unwrap();
    let head = coding.session().leaf_id().unwrap().to_owned();
    let messages = coding.agent().messages();
    faux.push(FauxResponse::Error("cannot summarize".into()));
    assert!(coding.compact(None).await.is_err());
    assert_eq!(coding.session().leaf_id(), Some(head.as_str()));
    assert_eq!(coding.agent().messages(), messages);
}

#[tokio::test]
async fn overflow_retries_once_after_a_committed_compaction() {
    let project = tempfile::tempdir().unwrap();
    let faux = FauxProvider::default();
    for response in [
        FauxResponse::text("old answer"),
        FauxResponse::Error("context_length_exceeded".into()),
        FauxResponse::text("compaction summary"),
        FauxResponse::Error("context_length_exceeded".into()),
    ] {
        faux.push(response);
    }
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(faux.model());
    let coding = CodingSession::new(options).unwrap();
    coding.prompt("old question").await.unwrap();
    coding.prompt("new question").await.unwrap();
    assert_eq!(faux.requests().len(), 4);
    assert!(coding.overflowed());
    assert_eq!(
        coding
            .session()
            .entries()
            .iter()
            .filter(|entry| matches!(entry.data, EntryData::Compaction { .. }))
            .count(),
        1
    );
    assert_eq!(
        coding.agent().messages(),
        coding.session().context().messages
    );
}

#[tokio::test]
async fn overflow_with_no_history_to_compact_preserves_the_error() {
    let project = tempfile::tempdir().unwrap();
    let faux = FauxProvider::default();
    faux.push(FauxResponse::Error("context_length_exceeded".into()));
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(faux.model());
    let coding = CodingSession::new(options).unwrap();
    coding.prompt("only question").await.unwrap();
    assert_eq!(faux.requests().len(), 1);
    assert!(coding.overflowed());
    assert_eq!(
        coding.agent().messages(),
        coding.session().context().messages
    );
}

#[tokio::test]
async fn an_empty_answer_at_the_length_limit_with_a_full_window_compacts_and_retries() {
    let project = tempfile::tempdir().unwrap();
    let faux = FauxProvider::default();
    let mut model = faux.model();
    model.context_window = 100_000;
    // A server that cuts the input to fit leaves no room for the answer.
    let mut full = zp_ai::Usage {
        input: 99_500,
        ..Default::default()
    };
    full.update_total();
    for response in [
        FauxResponse::text("old answer"),
        FauxResponse::Message {
            blocks: Vec::new(),
            stop_reason: Some(zp_ai::StopReason::Length),
            usage: full,
        },
        FauxResponse::text("compaction summary"),
        FauxResponse::text("final answer"),
    ] {
        faux.push(response);
    }
    let session = zp_session::Session::in_memory(project.path());
    let mut options = CodingSessionOptions::new(models(&faux), project.path(), session);
    options.model = Some(model);
    let coding = CodingSession::new(options).unwrap();
    coding.prompt("old question").await.unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = coding.agent().subscribe({
        let events = events.clone();
        move |event| match event {
            AgentEvent::ContextCompacting { .. } | AgentEvent::ContextCompacted { .. } => {
                events.lock().unwrap().push(event.name());
            }
            _ => {}
        }
    });
    let added = coding.prompt("new question").await.unwrap();

    assert_eq!(faux.pending(), 0, "summarized once and retried");
    assert_eq!(
        *events.lock().unwrap(),
        ["context_compacting", "context_compacted"],
        "clients are told when summarizing starts"
    );
    let last = added.last().and_then(AgentMessage::as_assistant).unwrap();
    assert_eq!(last.text(), "final answer");
    assert!(
        added
            .iter()
            .filter_map(AgentMessage::as_assistant)
            .all(|message| message.stop_reason != zp_ai::StopReason::Length),
        "the cut-off attempt is not part of the result"
    );
    assert!(!coding.overflowed());
    assert_eq!(
        coding.agent().messages(),
        coding.session().context().messages
    );
}
