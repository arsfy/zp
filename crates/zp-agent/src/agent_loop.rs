//! The agent loop: stream a response, run its tool calls, repeat while there is work.
//!
//! Ported from pi-agent-core's `runLoop`. The loop works on its own copy of the
//! transcript and reports everything through events; [`crate::Agent`] folds those events
//! into its state.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::join_all;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zp_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageStream, Context, Model, StopReason,
    StreamOptions, Tool, ToolCall, ToolResultMessage, now_ms,
};

use crate::event::AgentEvent;
use crate::hooks::{
    AfterToolCall, AgentHooks, BeforeModelRequest, BeforeToolCall, CompactionTrigger,
    ToolCallDecision,
};
use crate::message::AgentMessage;
use crate::schema::validate_arguments;
use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolExecutionMode, ToolRun};

/// Produces a response stream; [`crate::stream_fn`] adapts [`zp_ai::Models`].
pub type StreamFn =
    Arc<dyn Fn(&Model, &Context, StreamOptions) -> AssistantMessageStream + Send + Sync>;

/// Receives every event of a run, in order.
pub(crate) type Emit = Arc<dyn Fn(AgentEvent) + Send + Sync>;

pub(crate) const ABORTED: &str = "Request was aborted";
const TOOL_ABORTED: &str = "Operation aborted";

/// Everything a run needs, captured when it starts.
pub(crate) struct LoopConfig {
    pub run: Arc<ToolRun>,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub tool_execution: ToolExecutionMode,
    pub stream: StreamFn,
    pub request_options: StreamOptions,
    pub hooks: Arc<dyn AgentHooks>,
}

/// Sources of queued messages, polled at the points pi polls them.
pub(crate) struct LoopQueues {
    /// Messages to insert after the current turn.
    pub steering: Box<dyn FnMut() -> Vec<AgentMessage> + Send>,
    /// Messages to process once the agent would otherwise stop.
    pub follow_up: Box<dyn FnMut() -> Vec<AgentMessage> + Send>,
}

/// Runs the loop. `context` is the transcript before the run; `prompts` are appended
/// first (empty when continuing). Returns the messages the run added.
pub(crate) async fn run_loop(
    config: &LoopConfig,
    mut context: Vec<AgentMessage>,
    prompts: Vec<AgentMessage>,
    queues: &mut LoopQueues,
    emit: &Emit,
    cancel: &CancellationToken,
) -> Vec<AgentMessage> {
    let declarations: Vec<Tool> = config.tools.iter().map(|tool| tool.declaration()).collect();
    let mut added = Vec::new();
    let push = |context: &mut Vec<AgentMessage>, message: AgentMessage, added: &mut Vec<_>| {
        context.push(message.clone());
        added.push(message);
    };

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for message in prompts {
        emit_message(emit, &message);
        push(&mut context, message, &mut added);
    }

    // The user may have typed while the run was starting.
    let mut pending = poll(&mut queues.steering, cancel);
    let mut first_turn = true;
    let mut recovered_overflow = false;
    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            if !first_turn {
                emit(AgentEvent::TurnStart);
            }
            first_turn = false;
            for message in std::mem::take(&mut pending) {
                emit_message(emit, &message);
                push(&mut context, message, &mut added);
            }

            compact_context(
                config,
                &mut context,
                &declarations,
                CompactionTrigger::Threshold,
                emit,
                cancel,
            )
            .await;
            let message = stream_response(config, &context, &declarations, emit, cancel).await;
            push(&mut context, message.clone().into(), &mut added);
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                let may_recover = !recovered_overflow && message.stop_reason == StopReason::Error;
                emit(AgentEvent::TurnEnd {
                    message,
                    tool_results: Vec::new(),
                });
                if may_recover
                    && compact_context(
                        config,
                        &mut context,
                        &declarations,
                        CompactionTrigger::ProviderError,
                        emit,
                        cancel,
                    )
                    .await
                {
                    recovered_overflow = true;
                    continue;
                }
                emit(AgentEvent::AgentEnd {
                    messages: added.clone(),
                });
                return added;
            }

            let calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !calls.is_empty() {
                let batch = if message.stop_reason == StopReason::Length {
                    fail_truncated_calls(&calls, emit)
                } else {
                    execute_tool_calls(config, &context, &message, &calls, emit, cancel).await
                };
                has_more_tool_calls = !batch.terminate;
                for result in batch.results {
                    push(&mut context, result.clone().into(), &mut added);
                    tool_results.push(result);
                }
            }
            emit(AgentEvent::TurnEnd {
                message,
                tool_results,
            });
            pending = poll(&mut queues.steering, cancel);
        }

        // The agent would stop here; queued follow-ups start another turn.
        pending = poll(&mut queues.follow_up, cancel);
        if pending.is_empty() {
            break;
        }
    }
    emit(AgentEvent::AgentEnd {
        messages: added.clone(),
    });
    added
}

/// Queues are left alone once the run is aborted, so their messages survive for the
/// next run.
fn poll(
    source: &mut Box<dyn FnMut() -> Vec<AgentMessage> + Send>,
    cancel: &CancellationToken,
) -> Vec<AgentMessage> {
    if cancel.is_cancelled() {
        Vec::new()
    } else {
        source()
    }
}

fn emit_message(emit: &Emit, message: &AgentMessage) {
    emit(AgentEvent::MessageStart {
        message: message.clone(),
    });
    emit(AgentEvent::MessageEnd {
        message: message.clone(),
    });
}

/// An assistant message that failed before reaching the provider.
pub(crate) fn failed_message(model: &Model, reason: StopReason, error: &str) -> AssistantMessage {
    let mut message = AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);
    message.stop_reason = reason;
    message.error_message = Some(error.to_owned());
    message
}

/// Applies a committed context replacement, if the session can compact at this boundary.
async fn compact_context(
    config: &LoopConfig,
    context: &mut Vec<AgentMessage>,
    declarations: &[Tool],
    trigger: CompactionTrigger,
    emit: &Emit,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    let request = BeforeModelRequest {
        trigger,
        run: &config.run,
        messages: context,
        system_prompt: config.system_prompt.as_deref(),
        tools: declarations,
    };
    match config.hooks.compact_context(request, cancel).await {
        Ok(Some(compaction)) => {
            context.clone_from(&compaction.messages);
            emit(AgentEvent::ContextCompacted {
                compaction: Arc::new(compaction),
                trigger,
            });
            true
        }
        result => {
            if let Err(error) = result
                && !cancel.is_cancelled()
            {
                emit(AgentEvent::ContextCompactionFailed { error });
            }
            false
        }
    }
}

/// Streams one response, emitting message events. Never fails: errors come back as an
/// assistant message with stop reason `Error` or `Aborted`.
async fn stream_response(
    config: &LoopConfig,
    context: &[AgentMessage],
    declarations: &[Tool],
    emit: &Emit,
    cancel: &CancellationToken,
) -> AssistantMessage {
    let finish = |message: AssistantMessage, started: bool| {
        if !started {
            emit(AgentEvent::MessageStart {
                message: message.clone().into(),
            });
        }
        emit(AgentEvent::MessageEnd {
            message: message.clone().into(),
        });
        message
    };
    if cancel.is_cancelled() {
        return finish(
            failed_message(&config.run.model, StopReason::Aborted, ABORTED),
            false,
        );
    }

    let transformed = config.hooks.transform_context(context, cancel).await;
    if cancel.is_cancelled() {
        return finish(
            failed_message(&config.run.model, StopReason::Aborted, ABORTED),
            false,
        );
    }
    let messages = config
        .hooks
        .convert_to_llm(transformed.as_deref().unwrap_or(context));
    let request = Context {
        system_prompt: config.system_prompt.clone(),
        messages,
        tools: declarations.to_vec(),
    };
    let options = StreamOptions {
        thinking: config.run.thinking_level,
        cancel: cancel.clone(),
        ..config.request_options.clone()
    };

    let mut stream = (config.stream)(&config.run.model, &request, options);
    let mut started = false;
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Start { message } => {
                started = true;
                emit(AgentEvent::MessageStart {
                    message: message.into(),
                });
            }
            AssistantMessageEvent::Done { message } | AssistantMessageEvent::Error { message } => {
                return finish(message, started);
            }
            event if started => emit(AgentEvent::MessageUpdate { event }),
            _ => {}
        }
    }
    let reason = if cancel.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    finish(
        failed_message(
            &config.run.model,
            reason,
            "response stream ended unexpectedly",
        ),
        started,
    )
}

/// Results of one assistant message's tool calls.
struct Batch {
    results: Vec<ToolResultMessage>,
    terminate: bool,
}

/// A finished call.
#[derive(Clone)]
struct Finalized {
    result: AgentToolResult,
    duration_ms: Option<u64>,
}

impl Finalized {
    fn immediate(result: AgentToolResult) -> Self {
        Self {
            result,
            duration_ms: None,
        }
    }
}

/// A call after argument validation and `before_tool_call`.
enum Prepared<'a> {
    /// Resolved without running (unknown tool, invalid arguments, blocked, aborted).
    Immediate(AgentToolResult),
    /// Allowed to run.
    Ready {
        tool: &'a Arc<dyn AgentTool>,
        args: Value,
    },
}

fn start_event(call: &ToolCall) -> AgentEvent {
    AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: call.arguments.clone(),
    }
}

fn emit_end(emit: &Emit, call: &ToolCall, finalized: &Finalized) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.result.is_error,
        duration_ms: finalized.duration_ms,
    });
}

fn result_message(call: &ToolCall, finalized: Finalized) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: finalized.result.content,
        is_error: finalized.result.is_error,
        details: finalized.result.details,
        timestamp: now_ms(),
    }
}

fn emit_result_message(emit: &Emit, message: &ToolResultMessage) {
    emit_message(emit, &message.clone().into());
}

fn terminates(finalized: &[Finalized]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate)
}

/// A response cut off by the token limit may carry truncated arguments that still
/// parse; none of its calls are safe to run.
fn fail_truncated_calls(calls: &[ToolCall], emit: &Emit) -> Batch {
    let results = calls
        .iter()
        .map(|call| {
            emit(start_event(call));
            let finalized = Finalized::immediate(AgentToolResult::error(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so \
                 its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            )));
            emit_end(emit, call, &finalized);
            let message = result_message(call, finalized);
            emit_result_message(emit, &message);
            message
        })
        .collect();
    Batch {
        results,
        terminate: false,
    }
}

async fn execute_tool_calls(
    config: &LoopConfig,
    context: &[AgentMessage],
    message: &AssistantMessage,
    calls: &[ToolCall],
    emit: &Emit,
    cancel: &CancellationToken,
) -> Batch {
    let any_sequential = calls.iter().any(|call| {
        find_tool(config, &call.name)
            .is_some_and(|tool| tool.execution_mode() == Some(ToolExecutionMode::Sequential))
    });
    if config.tool_execution == ToolExecutionMode::Sequential || any_sequential {
        execute_sequential(config, context, message, calls, emit, cancel).await
    } else {
        execute_parallel(config, context, message, calls, emit, cancel).await
    }
}

async fn execute_sequential(
    config: &LoopConfig,
    context: &[AgentMessage],
    message: &AssistantMessage,
    calls: &[ToolCall],
    emit: &Emit,
    cancel: &CancellationToken,
) -> Batch {
    let mut finalized_calls = Vec::new();
    let mut results = Vec::new();
    for call in calls {
        emit(start_event(call));
        let finalized = match prepare(config, context, message, call, cancel).await {
            Prepared::Immediate(result) => Finalized::immediate(result),
            Prepared::Ready { tool, args } => {
                execute(config, context, message, call, tool, args, emit, cancel).await
            }
        };
        emit_end(emit, call, &finalized);
        finalized_calls.push(finalized.clone());
        let result = result_message(call, finalized);
        emit_result_message(emit, &result);
        results.push(result);
    }
    Batch {
        results,
        terminate: terminates(&finalized_calls),
    }
}

async fn execute_parallel(
    config: &LoopConfig,
    context: &[AgentMessage],
    message: &AssistantMessage,
    calls: &[ToolCall],
    emit: &Emit,
    cancel: &CancellationToken,
) -> Batch {
    // Each call is checked and run on its own, so a slow review or a pending approval
    // holds back only its call. Hooks that ask the user queue their questions themselves.
    for call in calls {
        emit(start_event(call));
    }
    let finalized = join_all(calls.iter().map(|call| async move {
        let finalized = match prepare(config, context, message, call, cancel).await {
            Prepared::Immediate(result) => Finalized::immediate(result),
            Prepared::Ready { .. } if cancel.is_cancelled() => {
                Finalized::immediate(AgentToolResult::error(TOOL_ABORTED))
            }
            Prepared::Ready { tool, args } => {
                execute(config, context, message, call, tool, args, emit, cancel).await
            }
        };
        emit_end(emit, call, &finalized);
        finalized
    }))
    .await;

    let terminate = terminates(&finalized);
    let results = calls
        .iter()
        .zip(finalized)
        .map(|(call, finalized)| {
            let result = result_message(call, finalized);
            emit_result_message(emit, &result);
            result
        })
        .collect();
    Batch { results, terminate }
}

fn find_tool<'a>(config: &'a LoopConfig, name: &str) -> Option<&'a Arc<dyn AgentTool>> {
    config.tools.iter().find(|tool| tool.name() == name)
}

async fn prepare<'a>(
    config: &'a LoopConfig,
    context: &[AgentMessage],
    message: &AssistantMessage,
    call: &ToolCall,
    cancel: &CancellationToken,
) -> Prepared<'a> {
    if cancel.is_cancelled() {
        return Prepared::Immediate(AgentToolResult::error(TOOL_ABORTED));
    }
    let Some(tool) = find_tool(config, &call.name) else {
        return Prepared::Immediate(AgentToolResult::error(format!(
            "Tool {} not found",
            call.name
        )));
    };
    let args = match validate_arguments(
        &tool.parameters(),
        tool.prepare_arguments(call.arguments.clone()),
    ) {
        Ok(args) => args,
        Err(problems) => {
            return Prepared::Immediate(AgentToolResult::error(format!(
                "Invalid arguments for tool \"{}\":\n{problems}",
                call.name
            )));
        }
    };
    let decision = config
        .hooks
        .before_tool_call(
            BeforeToolCall {
                run: &config.run,
                assistant_message: message,
                tool_call: call,
                args: &args,
                messages: context,
            },
            cancel,
        )
        .await;
    if cancel.is_cancelled() {
        return Prepared::Immediate(AgentToolResult::error(TOOL_ABORTED));
    }
    match decision {
        ToolCallDecision::Allow => Prepared::Ready { tool, args },
        ToolCallDecision::Block { reason, terminate } => Prepared::Immediate(AgentToolResult {
            terminate,
            ..AgentToolResult::error(reason.unwrap_or_else(|| "Tool execution was blocked".into()))
        }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    config: &LoopConfig,
    context: &[AgentMessage],
    message: &AssistantMessage,
    call: &ToolCall,
    tool: &Arc<dyn AgentTool>,
    args: Value,
    emit: &Emit,
    cancel: &CancellationToken,
) -> Finalized {
    let ctx = {
        let emit = emit.clone();
        let (id, name) = (call.id.clone(), call.name.clone());
        ToolContext::new(
            call.id.clone(),
            cancel.clone(),
            Some(config.run.clone()),
            move |partial_result| {
                emit(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    partial_result,
                });
            },
        )
    };
    let started = Instant::now();
    let outcome = AssertUnwindSafe(async { tool.execute(args.clone(), ctx.clone()).await })
        .catch_unwind()
        .await;
    ctx.close();
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let mut result = match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => AgentToolResult::error(format!("{error:#}")),
        Err(panic) => {
            tracing::error!(tool = call.name, "tool panicked");
            AgentToolResult::error(format!(
                "Tool {} panicked: {}",
                call.name,
                panic_message(panic.as_ref())
            ))
        }
    };
    let changes = config
        .hooks
        .after_tool_call(
            AfterToolCall {
                assistant_message: message,
                tool_call: call,
                args: &args,
                result: &result,
                messages: context,
            },
            cancel,
        )
        .await;
    if let Some(changes) = changes {
        changes.apply(&mut result);
    }
    Finalized {
        result,
        duration_ms: Some(duration_ms),
    }
}

pub(crate) fn panic_message(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}
