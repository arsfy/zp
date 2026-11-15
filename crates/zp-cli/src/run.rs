//! `zp-cli run`: stream one prompt (optionally with a demo tool loop).

use std::io::{IsTerminal as _, Read as _, Write as _};

use anyhow::{Result, bail};
use clap::Args;
use futures::StreamExt as _;
use serde_json::json;
use zp_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, Models,
    StopReason, StreamOptions, ThinkingLevel, Tool, ToolResultMessage,
};

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Model as `provider/model-id` (defaults to the first configured provider's default).
    #[arg(short, long)]
    model: Option<String>,
    /// Reasoning level: off, minimal, low, medium, high, xhigh, max.
    #[arg(short, long, default_value = "off")]
    thinking: ThinkingLevel,
    /// System prompt.
    #[arg(short, long)]
    system: Option<String>,
    /// Output token limit.
    #[arg(long)]
    max_tokens: Option<u64>,
    /// Override the model's API base URL (e.g. a proxy or local mock server).
    #[arg(long)]
    base_url: Option<String>,
    /// Offer a `get_current_time` tool and run tool calls until the model stops.
    #[arg(long)]
    demo_tool: bool,
    /// The prompt. Read from stdin when omitted.
    prompt: Vec<String>,
}

const MAX_TOOL_ROUNDS: usize = 5;

struct Style {
    enabled: bool,
}

impl Style {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

pub(crate) async fn run(models: &Models, args: RunArgs) -> Result<()> {
    let prompt = if args.prompt.is_empty() {
        if std::io::stdin().is_terminal() {
            bail!("no prompt given (pass it as an argument or pipe it on stdin)");
        }
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        input
    } else {
        args.prompt.join(" ")
    };
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }

    let mut model = crate::select_model(models, args.model.as_deref())?;
    if let Some(base_url) = args.base_url {
        model.base_url = base_url;
    }
    let style = Style {
        enabled: std::io::stderr().is_terminal(),
    };
    let thinking = model.clamp_thinking_level(args.thinking);
    eprintln!(
        "{}",
        style.paint(
            "2",
            &format!("{} · thinking {}", model.qualified_id(), thinking)
        )
    );

    let mut context = Context {
        system_prompt: args.system,
        messages: vec![Message::user(prompt)],
        tools: if args.demo_tool {
            vec![time_tool()]
        } else {
            Vec::new()
        },
    };

    let mut totals = (0u64, 0u64, 0u64, 0.0f64);
    for _ in 0..MAX_TOOL_ROUNDS {
        let options = StreamOptions {
            thinking: args.thinking,
            max_tokens: args.max_tokens,
            ..StreamOptions::default()
        };
        let cancel = options.cancel.clone();
        let stream = models.stream(&model, &context, options);
        let message = tokio::select! {
            message = print_stream(stream, &style) => message,
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                eprintln!("\n{}", style.paint("33", "aborted"));
                std::process::exit(130);
            }
        };
        totals.0 += message.usage.input;
        totals.1 += message.usage.cache_read + message.usage.cache_write;
        totals.2 += message.usage.output;
        totals.3 += message.usage.cost.total;

        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            let error = message
                .error_message
                .unwrap_or_else(|| "unknown error".into());
            eprintln!("{}", style.paint("31", &format!("error: {error}")));
            std::process::exit(1);
        }
        let calls: Vec<_> = message.tool_calls().cloned().collect();
        let stop_reason = message.stop_reason;
        context.messages.push(message.into());
        if calls.is_empty() || stop_reason != StopReason::ToolUse {
            break;
        }
        for call in calls {
            let (output, is_error) = if call.name == "get_current_time" {
                (current_time_utc(), false)
            } else {
                (format!("unknown tool `{}`", call.name), true)
            };
            eprintln!("{}", style.paint("36", &format!("← {output}")));
            context
                .messages
                .push(ToolResultMessage::text(&call, output, is_error).into());
        }
    }

    eprintln!(
        "{}",
        style.paint(
            "2",
            &format!(
                "tokens: in {} (+{} cached) · out {} · cost ${:.6}",
                totals.0, totals.1, totals.2, totals.3
            )
        )
    );
    Ok(())
}

/// Prints a response as it streams and returns the final message.
async fn print_stream(
    mut stream: zp_ai::AssistantMessageStream,
    style: &Style,
) -> AssistantMessage {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let mut ends_with_newline = true;
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::ThinkingStart { .. } => {
                let _ = write!(stderr, "{}", style.paint("2;3", "thinking: "));
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                let _ = write!(stderr, "{}", style.paint("2;3", &delta));
                let _ = stderr.flush();
            }
            AssistantMessageEvent::ThinkingEnd { .. } => {
                let _ = writeln!(stderr);
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                ends_with_newline = delta.ends_with('\n');
                let _ = write!(stdout, "{delta}");
                let _ = stdout.flush();
            }
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                if !ends_with_newline {
                    let _ = writeln!(stdout);
                    ends_with_newline = true;
                }
                let _ = writeln!(
                    stderr,
                    "{}",
                    style.paint(
                        "36",
                        &format!("→ {}({})", tool_call.name, tool_call.arguments)
                    )
                );
            }
            AssistantMessageEvent::Done { message } | AssistantMessageEvent::Error { message } => {
                let printed_text = message
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::Text(_)));
                if printed_text && !ends_with_newline {
                    let _ = writeln!(stdout);
                }
                return message;
            }
            _ => {}
        }
    }
    let mut message = AssistantMessage::empty("", "", "");
    message.stop_reason = StopReason::Error;
    message.error_message = Some("stream ended unexpectedly".into());
    message
}

fn time_tool() -> Tool {
    Tool {
        name: "get_current_time".into(),
        description: "Get the current date and time in UTC.".into(),
        parameters: json!({"type": "object", "properties": {}, "required": []}),
    }
}

/// Formats the current time as `YYYY-MM-DD HH:MM:SS UTC`.
fn current_time_utc() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
