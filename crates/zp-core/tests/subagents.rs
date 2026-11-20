use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use zp_agent::{AgentEvent, AgentMessage};
use zp_ai::{
    FauxBlock, FauxProvider, FauxResponse, Message, Models, StopReason, ThinkingLevel, Usage,
};
use zp_core::permissions::{
    Approver, Decision, PermissionGate, PermissionMode, PermissionPolicy, PermissionRequest,
};
use zp_core::subagents::{
    Approval, Assignment, Profile, Report, Status, SubagentSettings, TOOL_NAME,
};
use zp_core::{CodingSession, CodingSessionOptions};
use zp_session::Session;

fn profile(model: &FauxProvider) -> Profile {
    Profile {
        model: model.model().qualified_id(),
        description: "Locate evidence".into(),
        thinking: ThinkingLevel::Low,
    }
}

/// Enabled, otherwise the defaults (tasks start without asking).
fn settings() -> SubagentSettings {
    SubagentSettings {
        enabled: Some(true),
        ..SubagentSettings::default()
    }
}

fn coding(
    root: &Path,
    providers: &[&FauxProvider],
    settings: SubagentSettings,
    approver: Option<Arc<dyn Approver>>,
) -> CodingSession {
    let mut models = Models::new();
    for provider in providers {
        models.set_provider(Arc::new((*provider).clone()));
    }
    let mut options = CodingSessionOptions::new(models.clone(), root, Session::in_memory(root));
    options.model = Some(providers[0].model());
    options.subagents = settings;
    options.permissions = approver.map(|approver| {
        Arc::new(PermissionGate::new(
            PermissionPolicy {
                mode: PermissionMode::FullAuto,
                root: root.to_owned(),
                ..PermissionPolicy::default()
            },
            approver,
            models,
        ))
    });
    CodingSession::new(options).unwrap()
}

fn delegate(args: Value) -> FauxResponse {
    FauxResponse::tool_call(TOOL_NAME, args)
}

fn reports(messages: &[AgentMessage]) -> Vec<Report> {
    messages
        .iter()
        .filter_map(AgentMessage::as_tool_result)
        .filter_map(|message| message.details.as_ref().and_then(Report::from_details))
        .collect()
}

#[tokio::test]
async fn task_profiles_run_a_distinct_model_with_scoped_tools_and_accounted_usage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/allowed.txt"), "workspace evidence").unwrap();
    std::fs::write(root.join("excluded.txt"), "excluded content").unwrap();
    std::fs::write(dir.path().join("private.txt"), "outside content").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("private.txt"), root.join("src/link")).unwrap();

    let parent = FauxProvider::new("parent");
    let child = FauxProvider::new("child");
    let mut config = settings();
    config.assignment = Some(Assignment::TaskProfiles);
    config.default_profile = Some("explore".into());
    config.profiles.insert("explore".into(), profile(&child));
    parent.push(delegate(
        json!({"task": "find evidence", "paths": ["src"], "context": "explicit hint"}),
    ));
    parent.push(FauxResponse::text("Parent conclusion"));
    let reads = ["src/allowed.txt", "excluded.txt", "../private.txt"];
    let mut blocks: Vec<_> = reads
        .into_iter()
        .map(|path| FauxBlock::ToolCall {
            name: "read".into(),
            arguments: json!({"path": path}),
        })
        .collect();
    #[cfg(unix)]
    blocks.push(FauxBlock::ToolCall {
        name: "read".into(),
        arguments: json!({"path": "src/link"}),
    });
    child.push(FauxResponse::blocks(blocks));
    child.push(FauxResponse::text("Found evidence at src/allowed.txt:1"));

    let coding = coding(&root, &[&parent, &child], config, None);
    let run = coding.prompt("private parent transcript").await.unwrap();
    let reports = reports(&run);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].model, "child/faux-1");
    assert_eq!(reports[0].profile.as_deref(), Some("explore"));
    assert_eq!(reports[0].status, Status::Completed);
    assert_eq!(reports[0].usage.input, 20);
    assert_eq!(coding.usage().input, 40);
    assert_eq!(coding.usage().last_context_tokens, 15);

    let requests = child.requests();
    let child_tools: Vec<_> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(child_tools, ["read", "grep", "find", "ls"]);
    let child_prompt = serde_json::to_string(&requests[0].messages).unwrap();
    assert!(child_prompt.contains("explicit hint"));
    assert!(!child_prompt.contains("private parent transcript"));
    let results: Vec<_> = requests[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert!(!results[0].is_error);
    assert!(
        results[0].content[0]
            .as_text()
            .unwrap()
            .contains("workspace evidence")
    );
    assert!(results[1..].iter().all(|result| result.is_error));
    assert_eq!(parent.requests().len(), 2);
}

#[tokio::test]
async fn follow_main_model_rejects_overrides_and_applies_to_future_runs() {
    let dir = tempfile::tempdir().unwrap();
    let parent = FauxProvider::new("parent");
    let child = FauxProvider::new("child");
    let mut config = settings();
    config.assignment = Some(Assignment::TaskProfiles);
    config.default_profile = Some("explore".into());
    config.profiles.insert("explore".into(), profile(&child));
    let coding = coding(dir.path(), &[&parent, &child], config.clone(), None);
    config.assignment = Some(Assignment::FollowMainModel);
    coding.set_subagents(config);
    parent.push(delegate(
        json!({"task": "override model", "model": "child/faux-1"}),
    ));
    parent.push(delegate(json!({"task": "follow the main model"})));
    parent.push(FauxResponse::text("Child finding"));
    parent.push(FauxResponse::text("Main summary"));

    let result = coding.prompt("go").await.unwrap();
    let tools: Vec<_> = result
        .iter()
        .filter_map(AgentMessage::as_tool_result)
        .collect();
    assert!(tools[0].is_error);
    assert_eq!(reports(&result)[0].model, "parent/faux-1");
    assert_eq!(reports(&result)[0].profile, None);
    assert!(child.requests().is_empty());
    assert_eq!(parent.requests().len(), 4);
    coding.set_tools(zp_tools::coding_tools(dir.path()));
    assert!(
        coding
            .agent()
            .tools()
            .iter()
            .any(|tool| tool.name() == TOOL_NAME)
    );
    coding.set_subagents(SubagentSettings::default());
    assert!(
        coding
            .agent()
            .tools()
            .iter()
            .all(|tool| tool.name() != TOOL_NAME)
    );
}

#[derive(Default)]
struct Deny(Mutex<Vec<PermissionRequest>>);

impl Approver for Deny {
    fn approve(&self, request: PermissionRequest) -> BoxFuture<'static, Decision> {
        self.0.lock().unwrap().push(request);
        Box::pin(async { Decision::Deny(None) })
    }
}

#[tokio::test]
async fn asking_for_delegation_applies_even_in_full_auto() {
    let dir = tempfile::tempdir().unwrap();
    let parent = FauxProvider::default();
    let approver = Arc::new(Deny::default());
    parent.push(delegate(json!({"task": "find evidence"})));
    parent.push(FauxResponse::text("The user declined"));
    let coding = coding(
        dir.path(),
        &[&parent],
        SubagentSettings {
            approval: Some(Approval::Ask),
            ..settings()
        },
        Some(approver.clone()),
    );
    let messages = coding.prompt("go").await.unwrap();
    assert!(reports(&messages).is_empty());
    let requests = approver.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].summary.contains("faux/faux-1"));
    assert!(requests[0].grants.is_empty());
    assert_eq!(parent.requests().len(), 2);
}

#[tokio::test]
async fn request_and_task_limits_apply_and_reset_on_a_new_run() {
    let dir = tempfile::tempdir().unwrap();
    let parent = FauxProvider::new("parent");
    let child = FauxProvider::new("child");
    let mut config = settings();
    config.assignment = Some(Assignment::TaskProfiles);
    config.default_profile = Some("explore".into());
    config.profiles.insert("explore".into(), profile(&child));
    config.max_per_run = Some(1);
    config.max_model_requests_per_task = Some(1);
    parent.push(delegate(json!({"task": "first"})));
    parent.push(delegate(json!({"task": "over budget"})));
    parent.push(FauxResponse::text("Limit reached"));
    child.push(FauxResponse::tool_call("ls", json!({})));
    let coding = coding(dir.path(), &[&parent, &child], config, None);
    let first = coding.prompt("first main run").await.unwrap();
    let first_reports = reports(&first);
    assert_eq!(first_reports.len(), 1);
    assert_eq!(first_reports[0].status, Status::RequestLimit);
    assert_eq!(first_reports[0].model_requests, 1);
    assert_eq!(child.requests().len(), 1);
    let second_result = first
        .iter()
        .filter_map(AgentMessage::as_tool_result)
        .nth(1)
        .unwrap();
    assert!(
        second_result.content[0]
            .as_text()
            .unwrap()
            .contains("task limit")
    );

    parent.push(delegate(json!({"task": "within new budget"})));
    child.push(FauxResponse::text("Found it"));
    parent.push(FauxResponse::text("Done"));
    let second = coding.prompt("second main run").await.unwrap();
    assert_eq!(reports(&second)[0].status, Status::Completed);
}

#[tokio::test]
async fn cancelling_one_child_frees_the_parallel_slot_for_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let parent = FauxProvider::new("parent");
    let slow = FauxProvider::new("slow").with_chunk_delay(Duration::from_secs(60));
    let quick = FauxProvider::new("quick");
    let mut config = settings();
    config.assignment = Some(Assignment::TaskProfiles);
    config.max_concurrent = Some(1);
    config.profiles.insert("slow".into(), profile(&slow));
    config.profiles.insert("quick".into(), profile(&quick));
    parent.push(FauxResponse::blocks(vec![
        FauxBlock::ToolCall {
            name: TOOL_NAME.into(),
            arguments: json!({"task": "slow task", "profile": "slow"}),
        },
        FauxBlock::ToolCall {
            name: TOOL_NAME.into(),
            arguments: json!({"task": "quick task", "profile": "quick"}),
        },
    ]));
    slow.push(FauxResponse::text("Slow finding"));
    quick.push(FauxResponse::text("Quick finding"));
    parent.push(FauxResponse::text("Main run finished"));
    let coding = Arc::new(coding(dir.path(), &[&parent, &slow, &quick], config, None));
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let _subscription = coding.agent().subscribe(move |event| {
        if let AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } = event
            && let Some(report) = partial_result
                .details
                .as_ref()
                .and_then(Report::from_details)
        {
            let _ = send.send((tool_call_id.clone(), report));
        }
    });
    let task = tokio::spawn({
        let coding = coding.clone();
        async move { coding.prompt("run both").await.unwrap() }
    });
    let slow_id = tokio::time::timeout(Duration::from_secs(5), async {
        let mut slow_id = None;
        while let Some((id, report)) = receive.recv().await {
            if report.model == "slow/faux-1" && report.status == Status::Running {
                slow_id = Some(id);
            }
            if report.model == "quick/faux-1" && report.status == Status::Queued {
                return slow_id.unwrap();
            }
        }
        panic!("no queued subagent");
    })
    .await
    .unwrap();
    assert!(quick.requests().is_empty());
    coding.cancel_subagent(&slow_id);
    let messages = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let reports = reports(&messages);
    assert_eq!(
        reports
            .iter()
            .map(|report| report.status)
            .collect::<Vec<_>>(),
        [Status::Cancelled, Status::Completed]
    );
    assert_eq!(parent.requests().len(), 2);
}

#[tokio::test]
async fn children_stop_on_timeout_or_parent_cancellation() {
    for stop_parent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let parent = FauxProvider::new("parent");
        let child = FauxProvider::new("slow").with_chunk_delay(Duration::from_secs(60));
        let mut config = settings();
        config.assignment = Some(Assignment::TaskProfiles);
        config.default_profile = Some("explore".into());
        config.profiles.insert("explore".into(), profile(&child));
        config.timeout_seconds = Some(if stop_parent { 60 } else { 1 });
        parent.push(delegate(json!({"task": "slow child"})));
        parent.push(FauxResponse::text("Main run can finish after a timeout"));
        child.push(FauxResponse::text("Slow finding"));
        let coding = Arc::new(coding(dir.path(), &[&parent, &child], config, None));
        let (actions, mut progress) = tokio::sync::mpsc::unbounded_channel();
        let _subscription = coding.agent().subscribe(move |event| {
            if let AgentEvent::ToolExecutionUpdate { partial_result, .. } = event
                && partial_result
                    .details
                    .as_ref()
                    .and_then(Report::from_details)
                    .is_some_and(|report| report.status == Status::Running)
            {
                let _ = actions.send(());
            }
        });
        let task = tokio::spawn({
            let coding = coding.clone();
            async move { coding.prompt("start child").await.unwrap() }
        });
        tokio::time::timeout(Duration::from_secs(5), progress.recv())
            .await
            .unwrap()
            .unwrap();
        if stop_parent {
            coding.agent().abort();
        }
        let messages = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reports(&messages)[0].status,
            if stop_parent {
                Status::Cancelled
            } else {
                Status::TimedOut
            }
        );
        assert_eq!(parent.requests().len(), if stop_parent { 1 } else { 2 });
    }
}

#[tokio::test]
async fn time_spent_waiting_for_a_slot_does_not_count_toward_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let parent = FauxProvider::new("parent");
    // The first task holds the only slot for most of the timeout.
    let slow = FauxProvider::new("slow").with_chunk_delay(Duration::from_millis(600));
    let quick = FauxProvider::new("quick").with_chunk_delay(Duration::from_millis(100));
    let mut config = settings();
    config.assignment = Some(Assignment::TaskProfiles);
    config.max_concurrent = Some(1);
    config.timeout_seconds = Some(2);
    config.profiles.insert("slow".into(), profile(&slow));
    config.profiles.insert("quick".into(), profile(&quick));
    parent.push(FauxResponse::blocks(vec![
        FauxBlock::ToolCall {
            name: TOOL_NAME.into(),
            arguments: json!({"task": "slow task", "profile": "slow"}),
        },
        FauxBlock::ToolCall {
            name: TOOL_NAME.into(),
            arguments: json!({"task": "quick task", "profile": "quick"}),
        },
    ]));
    slow.push(FauxResponse::text("Slow finding"));
    quick.push(FauxResponse::text("Quick finding"));
    parent.push(FauxResponse::text("Main run finished"));
    let coding = coding(dir.path(), &[&parent, &slow, &quick], config, None);
    let reports = reports(&coding.prompt("run both").await.unwrap());
    assert_eq!(
        reports
            .iter()
            .map(|report| report.status)
            .collect::<Vec<_>>(),
        [Status::Completed, Status::Completed]
    );
    assert!(
        reports[1].elapsed_ms < 1000,
        "the queued task's time excludes the wait: {}ms",
        reports[1].elapsed_ms
    );
}

#[tokio::test]
async fn a_conclusion_cut_off_at_the_output_limit_is_returned_marked_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let parent = FauxProvider::new("parent");
    parent.push(delegate(json!({"task": "explain everything"})));
    parent.push(FauxResponse::Message {
        blocks: vec![FauxBlock::Text("Partial finding".into())],
        stop_reason: Some(StopReason::Length),
        usage: Usage::default(),
    });
    parent.push(FauxResponse::text("Main run finished"));
    let mut config = settings();
    config.max_output_tokens = Some(50_000);
    let coding = coding(dir.path(), &[&parent], config, None);
    let messages = coding.prompt("delegate").await.unwrap();
    assert_eq!(reports(&messages)[0].status, Status::Truncated);
    let result = messages
        .iter()
        .find_map(AgentMessage::as_tool_result)
        .unwrap();
    assert!(!result.is_error, "a cut-off conclusion is still a result");
    let text = result.content[0].as_text().unwrap();
    assert!(text.starts_with("Partial finding"), "{text}");
    assert!(text.contains("output token limit"), "{text}");
}
