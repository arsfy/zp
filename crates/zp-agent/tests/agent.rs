//! Agent loop scenarios against the faux provider.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zp_agent::{
    AfterToolCall, Agent, AgentError, AgentEvent, AgentHooks, AgentMessage, AgentOptions,
    AgentTool, AgentToolResult, BeforeToolCall, CustomMessage, QueueMode, ToolCallDecision,
    ToolContext, ToolExecutionMode, ToolResultOverride,
};
use zp_ai::{
    FauxBlock, FauxProvider, FauxResponse, Message, Models, StopReason, Usage, UserContent,
};

// ----- fixtures --------------------------------------------------------------------------

fn agent_with(faux: &FauxProvider, tools: Vec<Arc<dyn AgentTool>>) -> Agent {
    let mut models = Models::new();
    models.set_provider(Arc::new(faux.clone()));
    let mut options = AgentOptions::with_models(models, faux.model());
    options.system_prompt = Some("You are a test.".into());
    options.tools = tools;
    Agent::new(options)
}

/// Records event names (and a few details) in order.
fn record(agent: &Agent) -> (Arc<Mutex<Vec<String>>>, zp_agent::Subscription) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let subscription = agent.subscribe(move |event| {
        let entry = match event {
            AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => {
                format!("{}:{}", event.name(), role(message))
            }
            AgentEvent::ToolExecutionStart { tool_name, .. }
            | AgentEvent::ToolExecutionUpdate { tool_name, .. }
            | AgentEvent::ToolExecutionEnd { tool_name, .. } => {
                format!("{}:{tool_name}", event.name())
            }
            _ => event.name().to_owned(),
        };
        sink.lock().unwrap().push(entry);
    });
    (log, subscription)
}

fn role(message: &AgentMessage) -> &'static str {
    match message {
        AgentMessage::Llm(Message::User(_)) => "user",
        AgentMessage::Llm(Message::Assistant(_)) => "assistant",
        AgentMessage::Llm(Message::ToolResult(_)) => "toolResult",
        AgentMessage::Custom(_) => "custom",
    }
}

fn roles(messages: &[AgentMessage]) -> Vec<&'static str> {
    messages.iter().map(role).collect()
}

fn text_of(content: &[UserContent]) -> String {
    content.iter().filter_map(UserContent::as_text).collect()
}

fn tool_results(messages: &[AgentMessage]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter_map(AgentMessage::as_tool_result)
        .map(|r| (r.tool_name.clone(), text_of(&r.content), r.is_error))
        .collect()
}

fn calls(names: &[(&str, Value)]) -> FauxResponse {
    FauxResponse::blocks(
        names
            .iter()
            .map(|(name, arguments)| FauxBlock::ToolCall {
                name: (*name).into(),
                arguments: arguments.clone(),
            })
            .collect(),
    )
}

/// Waits (bounded) for an event matching `predicate`.
async fn wait_for(
    events: &mut zp_agent::AgentEvents,
    predicate: impl Fn(&AgentEvent) -> bool,
) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.next().await.expect("event stream ended");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

/// `echo {text}` → `text`, optionally after a delay, reporting progress once.
struct Echo {
    delay: Duration,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    mode: Option<ToolExecutionMode>,
}

impl Echo {
    fn new() -> Self {
        Self {
            delay: Duration::ZERO,
            active: Arc::default(),
            max_active: Arc::default(),
            mode: None,
        }
    }
}

impl AgentTool for Echo {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo text back."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}, "delay_ms": {"type": "integer"}},
            "required": ["text"]
        })
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.mode
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            ctx.update(AgentToolResult::text("working"));
            let delay = args["delay_ms"]
                .as_u64()
                .map_or(self.delay, Duration::from_millis);
            tokio::time::sleep(delay).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(
                AgentToolResult::text(args["text"].as_str().unwrap_or_default())
                    .with_details(json!({"echoed": true})),
            )
        })
    }
}

/// Blocks until released or cancelled.
struct Gate {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        })
    }
}

impl AgentTool for Gate {
    fn name(&self) -> &str {
        "gate"
    }

    fn description(&self) -> &str {
        "Waits for the test."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn execute<'a>(
        &'a self,
        _args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            self.started.notify_one();
            tokio::select! {
                () = self.release.notified() => Ok(AgentToolResult::text("released")),
                () = ctx.cancel.cancelled() => anyhow::bail!("cancelled by user"),
            }
        })
    }
}

/// Fails in configurable ways.
struct Broken;

impl AgentTool for Broken {
    fn name(&self) -> &str {
        "broken"
    }

    fn description(&self) -> &str {
        "Fails."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"how": {"type": "string", "enum": ["err", "panic", "flag"]}}, "required": ["how"]})
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            match args["how"].as_str() {
                Some("err") => Err(anyhow::anyhow!("disk full").context("writing file")),
                Some("panic") => panic!("tool bug"),
                _ => Ok(AgentToolResult::error("soft failure").with_details(json!({"code": 7}))),
            }
        })
    }
}

// ----- scenarios -------------------------------------------------------------------------

#[tokio::test]
async fn text_prompt_emits_the_full_lifecycle() {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("Hello!"));
    let agent = agent_with(&faux, Vec::new());
    let (log, _subscription) = record(&agent);

    let added = agent.prompt("hi").await.unwrap();
    assert_eq!(roles(&added), ["user", "assistant"]);
    assert_eq!(added[1].as_assistant().unwrap().text(), "Hello!");
    assert_eq!(agent.messages(), added);
    assert!(!agent.is_streaming());
    assert!(agent.streaming_message().is_none());

    let log = log.lock().unwrap().clone();
    let without_updates: Vec<&str> = log
        .iter()
        .map(String::as_str)
        .filter(|e| *e != "message_update")
        .collect();
    assert_eq!(
        without_updates,
        [
            "agent_start",
            "turn_start",
            "message_start:user",
            "message_end:user",
            "model_request_start",
            "message_start:assistant",
            "message_end:assistant",
            "turn_end",
            "agent_end"
        ]
    );
    assert!(log.iter().filter(|e| *e == "message_update").count() >= 3);

    let request = &faux.requests()[0];
    assert_eq!(request.system_prompt.as_deref(), Some("You are a test."));
    assert_eq!(request.messages.len(), 1);
}

#[tokio::test]
async fn multi_turn_tool_calls() {
    let faux = FauxProvider::default();
    faux.push(calls(&[("echo", json!({"text": "one"}))]));
    faux.push(calls(&[
        ("echo", json!({"text": "two"})),
        ("echo", json!({"text": "three"})),
    ]));
    faux.push(FauxResponse::text("done"));
    let agent = agent_with(&faux, vec![Arc::new(Echo::new())]);
    let (log, _subscription) = record(&agent);

    let added = agent.prompt("go").await.unwrap();
    assert_eq!(
        roles(&added),
        [
            "user",
            "assistant",
            "toolResult",
            "assistant",
            "toolResult",
            "toolResult",
            "assistant"
        ]
    );
    assert_eq!(
        tool_results(&added),
        [
            ("echo".into(), "one".into(), false),
            ("echo".into(), "two".into(), false),
            ("echo".into(), "three".into(), false)
        ]
    );
    let first_result = added[2].as_tool_result().unwrap();
    assert_eq!(first_result.details, Some(json!({"echoed": true})));

    // Every request carries the tool declarations and the transcript so far.
    let requests = faux.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].tools[0].name, "echo");
    assert_eq!(requests[2].messages.len(), 6);

    let log = log.lock().unwrap();
    let count = |name: &str| log.iter().filter(|e| *e == name).count();
    assert_eq!(count("turn_start"), 3);
    assert_eq!(count("model_request_start"), 3);
    assert_eq!(count("turn_end"), 3);
    assert_eq!(count("tool_execution_start:echo"), 3);
    assert_eq!(count("tool_execution_update:echo"), 3);
    assert_eq!(count("tool_execution_end:echo"), 3);
    assert_eq!(faux.pending(), 0);
}

#[tokio::test]
async fn parallel_tools_overlap_and_keep_result_order() {
    let faux = FauxProvider::default();
    faux.push(calls(&[
        ("echo", json!({"text": "slow", "delay_ms": 80})),
        ("echo", json!({"text": "fast", "delay_ms": 0})),
    ]));
    faux.push(FauxResponse::text("ok"));
    let echo = Arc::new(Echo::new());
    let agent = agent_with(&faux, vec![echo.clone()]);
    let ends = Arc::new(Mutex::new(Vec::new()));
    let sink = ends.clone();
    let _subscription = agent.subscribe(move |event| {
        if let AgentEvent::ToolExecutionEnd {
            result,
            duration_ms,
            ..
        } = event
        {
            assert!(duration_ms.is_some());
            sink.lock().unwrap().push(text_of(&result.content));
        }
    });

    let added = agent.prompt("go").await.unwrap();
    assert_eq!(echo.max_active.load(Ordering::SeqCst), 2);
    // Completion order for events, call order for messages.
    assert_eq!(*ends.lock().unwrap(), ["fast", "slow"]);
    let results: Vec<String> = tool_results(&added).into_iter().map(|r| r.1).collect();
    assert_eq!(results, ["slow", "fast"]);
}

#[tokio::test]
async fn sequential_mode_runs_one_at_a_time() {
    for (agent_mode, tool_mode) in [
        (ToolExecutionMode::Sequential, None),
        (
            ToolExecutionMode::Parallel,
            Some(ToolExecutionMode::Sequential),
        ),
    ] {
        let faux = FauxProvider::default();
        faux.push(calls(&[
            ("echo", json!({"text": "a", "delay_ms": 20})),
            ("echo", json!({"text": "b", "delay_ms": 0})),
        ]));
        faux.push(FauxResponse::text("ok"));
        let echo = Arc::new(Echo {
            mode: tool_mode,
            ..Echo::new()
        });
        let agent = agent_with(&faux, vec![echo.clone()]);
        agent.set_tool_execution(agent_mode);
        let (log, _subscription) = record(&agent);

        agent.prompt("go").await.unwrap();
        assert_eq!(echo.max_active.load(Ordering::SeqCst), 1);
        let log = log.lock().unwrap();
        let tool_events: Vec<&str> = log
            .iter()
            .map(String::as_str)
            .filter(|e| {
                e.starts_with("tool_execution_start")
                    || e.starts_with("tool_execution_end")
                    || *e == "message_end:toolResult"
            })
            .collect();
        assert_eq!(
            tool_events,
            [
                "tool_execution_start:echo",
                "tool_execution_end:echo",
                "message_end:toolResult",
                "tool_execution_start:echo",
                "tool_execution_end:echo",
                "message_end:toolResult"
            ]
        );
    }
}

#[tokio::test]
async fn failures_become_error_results_and_the_loop_continues() {
    let faux = FauxProvider::default();
    faux.push(calls(&[
        ("missing", json!({})),
        ("echo", json!({"text": 5, "delay_ms": "soon"})),
        ("broken", json!({"how": "err"})),
        ("broken", json!({"how": "panic"})),
        ("broken", json!({"how": "flag"})),
    ]));
    faux.push(FauxResponse::text("recovered"));
    let agent = agent_with(&faux, vec![Arc::new(Echo::new()), Arc::new(Broken)]);

    let added = agent.prompt("go").await.unwrap();
    let results = tool_results(&added);
    assert_eq!(
        results[0],
        ("missing".into(), "Tool missing not found".into(), true)
    );
    assert_eq!(
        results[1],
        (
            "echo".into(),
            "Invalid arguments for tool \"echo\":\ndelay_ms: expected integer, got string".into(),
            true
        )
    );
    assert_eq!(
        results[2],
        ("broken".into(), "writing file: disk full".into(), true)
    );
    assert_eq!(
        results[3],
        (
            "broken".into(),
            "Tool broken panicked: tool bug".into(),
            true
        )
    );
    assert_eq!(results[4], ("broken".into(), "soft failure".into(), true));
    assert_eq!(
        added[6].as_tool_result().unwrap().details,
        Some(json!({"code": 7}))
    );
    assert_eq!(
        added.last().unwrap().as_assistant().unwrap().text(),
        "recovered"
    );
}

struct Policy {
    seen_args: Mutex<Vec<Value>>,
}

impl AgentHooks for Policy {
    fn before_tool_call<'a>(
        &'a self,
        call: BeforeToolCall<'a>,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ToolCallDecision> {
        Box::pin(async move {
            self.seen_args.lock().unwrap().push(call.args.clone());
            // The transcript at decision time ends with the requesting assistant message.
            assert!(call.messages.last().unwrap().is_assistant());
            match call.args["text"].as_str() {
                Some("forbidden") => ToolCallDecision::block("not allowed here"),
                Some("stop") => ToolCallDecision::Block {
                    reason: None,
                    terminate: true,
                },
                _ => ToolCallDecision::Allow,
            }
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        call: AfterToolCall<'a>,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<ToolResultOverride>> {
        Box::pin(async move {
            let text = text_of(&call.result.content);
            Some(ToolResultOverride {
                content: Some(vec![UserContent::text(format!("[checked] {text}"))]),
                ..ToolResultOverride::default()
            })
        })
    }
}

#[tokio::test]
async fn hooks_block_rewrite_and_terminate() {
    let faux = FauxProvider::default();
    faux.push(calls(&[
        ("echo", json!({"text": "fine"})),
        ("echo", json!({"text": "forbidden"})),
    ]));
    faux.push(calls(&[("echo", json!({"text": "stop"}))]));
    faux.push(FauxResponse::text("never requested"));
    let agent = agent_with(&faux, vec![Arc::new(Echo::new())]);
    let policy = Arc::new(Policy {
        seen_args: Mutex::new(Vec::new()),
    });
    agent.set_hooks(policy.clone());

    let added = agent.prompt("go").await.unwrap();
    assert_eq!(
        tool_results(&added),
        [
            ("echo".into(), "[checked] fine".into(), false),
            ("echo".into(), "not allowed here".into(), true),
            ("echo".into(), "Tool execution was blocked".into(), true),
        ]
    );
    // Every result of the last batch asked to terminate, so no third request was made.
    assert_eq!(faux.requests().len(), 2);
    assert_eq!(faux.pending(), 1);
    assert!(added.last().unwrap().as_tool_result().is_some());
    assert_eq!(policy.seen_args.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn abort_mid_stream() {
    let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(30));
    faux.push(FauxResponse::text("a long answer that streams slowly"));
    faux.push(FauxResponse::text("unused"));
    let agent = agent_with(&faux, Vec::new());
    agent.follow_up("queued for later");
    let mut events = agent.events();

    let runner = agent.clone();
    let run = tokio::spawn(async move { runner.prompt("hi").await });
    wait_for(&mut events, |e| {
        matches!(
            e,
            AgentEvent::MessageUpdate {
                event: zp_ai::AssistantMessageEvent::TextDelta { .. }
            }
        )
    })
    .await;
    let partial = agent.streaming_message().expect("streaming");
    assert!(agent.is_streaming());
    assert_eq!(partial.provider, "faux");
    assert!(!partial.text().is_empty());
    agent.abort();

    let added = run.await.unwrap().unwrap();
    let last = added.last().unwrap().as_assistant().unwrap();
    assert_eq!(last.stop_reason, StopReason::Aborted);
    assert!(!last.text().is_empty(), "partial content is kept");
    assert_eq!(
        agent.error_message().as_deref(),
        Some("Request was aborted")
    );
    assert!(!agent.is_streaming());
    // Queued messages survive an abort.
    assert_eq!(agent.follow_up_queue().len(), 1);
    assert_eq!(faux.requests().len(), 1);
    wait_for(&mut events, |e| matches!(e, AgentEvent::AgentEnd { .. })).await;
}

#[tokio::test]
async fn abort_during_tool_execution() {
    let faux = FauxProvider::default();
    faux.push(calls(&[
        ("gate", json!({})),
        ("echo", json!({"text": "x"})),
    ]));
    faux.push(FauxResponse::text("unused"));
    let gate = Gate::new();
    let agent = agent_with(&faux, vec![gate.clone(), Arc::new(Echo::new())]);
    agent.set_tool_execution(ToolExecutionMode::Sequential);

    let runner = agent.clone();
    let run = tokio::spawn(async move { runner.prompt("go").await });
    gate.started.notified().await;
    assert_eq!(agent.pending_tool_calls().len(), 1);
    agent.abort();
    let added = run.await.unwrap().unwrap();

    assert_eq!(
        roles(&added),
        ["user", "assistant", "toolResult", "toolResult", "assistant"]
    );
    assert_eq!(
        tool_results(&added),
        [
            ("gate".into(), "cancelled by user".into(), true),
            ("echo".into(), "Operation aborted".into(), true),
        ]
    );
    let last = added.last().unwrap().as_assistant().unwrap();
    assert_eq!(last.stop_reason, StopReason::Aborted);
    // The aborted turn is closed locally, without another provider request.
    assert_eq!(faux.requests().len(), 1);
    assert!(agent.pending_tool_calls().is_empty());
    agent.wait_for_idle().await;
}

#[tokio::test]
async fn steering_is_inserted_after_the_current_tool_batch() {
    let faux = FauxProvider::default();
    faux.push(calls(&[("gate", json!({}))]));
    faux.push(FauxResponse::text("adjusted"));
    faux.push(FauxResponse::text("adjusted again"));
    let gate = Gate::new();
    let agent = agent_with(&faux, vec![gate.clone()]);

    let runner = agent.clone();
    let run = tokio::spawn(async move { runner.prompt("start").await });
    gate.started.notified().await;
    agent.steer("change of plan");
    agent.steer("and another thing");
    assert!(agent.has_queued_messages());
    gate.release.notify_one();
    let added = run.await.unwrap().unwrap();

    // One-at-a-time: each steering message gets its own turn, after the tool result.
    assert_eq!(
        roles(&added),
        [
            "user",
            "assistant",
            "toolResult",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    let requests = faux.requests();
    assert_eq!(requests.len(), 3);
    let Message::User(steer) = requests[1].messages.last().unwrap() else {
        panic!("expected the steering message last")
    };
    assert_eq!(text_of(&steer.content), "change of plan");
    assert!(!agent.has_queued_messages());
}

#[tokio::test]
async fn steering_all_mode_and_follow_ups() {
    let faux = FauxProvider::default();
    faux.push(calls(&[("gate", json!({}))]));
    faux.push(FauxResponse::text("both steering messages seen"));
    faux.push(FauxResponse::text("follow-up handled"));
    let gate = Gate::new();
    let agent = agent_with(&faux, vec![gate.clone()]);
    agent.set_steering_mode(QueueMode::All);

    let runner = agent.clone();
    let run = tokio::spawn(async move { runner.prompt("start").await });
    gate.started.notified().await;
    agent.follow_up("after you are done");
    agent.steer("first");
    agent.steer("second");
    gate.release.notify_one();
    let added = run.await.unwrap().unwrap();

    assert_eq!(
        roles(&added),
        [
            "user",
            "assistant",
            "toolResult",
            "user",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    let Message::User(follow_up) = faux.requests()[2].messages.last().unwrap().clone() else {
        panic!()
    };
    assert_eq!(text_of(&follow_up.content), "after you are done");
}

#[tokio::test]
async fn continue_run_and_busy_errors() {
    let faux = FauxProvider::default().with_chunk_delay(Duration::from_millis(20));
    faux.push(FauxResponse::Error("overloaded".into()));
    faux.push(FauxResponse::text("second try"));
    faux.push(FauxResponse::text("slow"));
    let agent = agent_with(&faux, Vec::new());

    assert_eq!(
        agent.continue_run().await,
        Err(AgentError::NothingToContinue)
    );
    assert_eq!(agent.prompt(Vec::new()).await, Err(AgentError::EmptyPrompt));

    let added = agent.prompt("hi").await.unwrap();
    let failed = added.last().unwrap().as_assistant().unwrap();
    assert_eq!(failed.stop_reason, StopReason::Error);
    assert_eq!(agent.error_message().as_deref(), Some("overloaded"));
    assert_eq!(
        agent.continue_run().await,
        Err(AgentError::CannotContinueFromAssistant)
    );

    // Retry: drop the failed response and continue from the user message.
    let mut messages = agent.messages();
    messages.pop();
    agent.set_messages(messages);
    let added = agent.continue_run().await.unwrap();
    assert_eq!(roles(&added), ["assistant"]);
    assert_eq!(added[0].as_assistant().unwrap().text(), "second try");
    assert!(agent.error_message().is_none());

    // Continuing after an assistant message uses queued steering as the prompt.
    agent.steer("more please");
    let mut events = agent.events();
    let runner = agent.clone();
    let run = tokio::spawn(async move { runner.continue_run().await });
    wait_for(&mut events, |e| matches!(e, AgentEvent::AgentStart)).await;
    assert_eq!(agent.prompt("again").await, Err(AgentError::Busy));
    assert_eq!(agent.reset(), Err(AgentError::Busy));
    let added = run.await.unwrap().unwrap();
    assert_eq!(roles(&added), ["user", "assistant"]);

    agent.reset().unwrap();
    assert!(agent.messages().is_empty());
}

struct ContextHooks;

impl AgentHooks for ContextHooks {
    fn transform_context<'a>(
        &'a self,
        messages: &'a [AgentMessage],
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<Vec<AgentMessage>>> {
        // Keep only the last two entries, like a crude compaction.
        Box::pin(async move { Some(messages[messages.len().saturating_sub(2)..].to_vec()) })
    }

    fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        messages
            .iter()
            .map(|message| match message {
                AgentMessage::Llm(message) => message.clone(),
                AgentMessage::Custom(custom) => Message::user(format!(
                    "[{}] {}",
                    custom.custom_type,
                    text_of(&custom.content)
                )),
            })
            .collect()
    }
}

#[tokio::test]
async fn transform_context_and_convert_to_llm() {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::text("ok"));
    let agent = agent_with(&faux, Vec::new());
    agent.set_messages(vec![
        AgentMessage::user("old 1"),
        AgentMessage::user("old 2"),
        CustomMessage::text("summary", "earlier work").into(),
    ]);
    agent.set_hooks(Arc::new(ContextHooks));

    agent.prompt("now").await.unwrap();
    let request = &faux.requests()[0];
    let texts: Vec<String> = request
        .messages
        .iter()
        .map(|m| match m {
            Message::User(user) => text_of(&user.content),
            _ => String::new(),
        })
        .collect();
    assert_eq!(texts, ["[summary] earlier work", "now"]);
    // The stored transcript is untouched.
    assert_eq!(agent.messages().len(), 5);
}

#[tokio::test]
async fn truncated_tool_calls_are_not_executed() {
    let faux = FauxProvider::default();
    faux.push(FauxResponse::Message {
        blocks: vec![FauxBlock::ToolCall {
            name: "echo".into(),
            arguments: json!({"text": "cut"}),
        }],
        stop_reason: Some(StopReason::Length),
        usage: Usage::default(),
    });
    faux.push(FauxResponse::text("retrying"));
    let echo = Arc::new(Echo::new());
    let agent = agent_with(&faux, vec![echo.clone()]);

    let added = agent.prompt("go").await.unwrap();
    let results = tool_results(&added);
    assert!(results[0].2);
    assert!(results[0].1.contains("output token limit"));
    assert_eq!(echo.max_active.load(Ordering::SeqCst), 0);
    assert_eq!(
        added.last().unwrap().as_assistant().unwrap().text(),
        "retrying"
    );
}

#[tokio::test]
async fn listeners_can_call_back_into_the_agent() {
    let faux = FauxProvider::default();
    faux.push(calls(&[("echo", json!({"text": "x"}))]));
    faux.push(FauxResponse::text("done"));
    let agent = agent_with(&faux, vec![Arc::new(Echo::new())]);
    let observer = agent.clone();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = seen.clone();
    let subscription = agent.subscribe(move |event| {
        let state = observer.state();
        if let AgentEvent::MessageEnd { .. } = event {
            // State already includes the finished message.
            assert!(!state.messages.is_empty());
        }
        if let AgentEvent::ToolExecutionStart { tool_call_id, .. } = event {
            assert!(state.pending_tool_calls.contains(tool_call_id));
        }
        counter.fetch_add(1, Ordering::SeqCst);
    });
    agent.prompt("go").await.unwrap();
    let total = seen.load(Ordering::SeqCst);
    assert!(total > 10);

    drop(subscription);
    faux.push(FauxResponse::text("again"));
    agent.prompt("again").await.unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), total, "unsubscribed on drop");
}

#[test]
fn run_futures_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let faux = FauxProvider::default();
    let agent = agent_with(&faux, Vec::new());
    assert_send(&agent.prompt("x"));
    assert_send(&agent.continue_run());
    assert_send(&agent.wait_for_idle());
}
