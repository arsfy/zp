//! `zp-cli run`: stream one prompt through the agent loop (optionally with a demo tool).

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use clap::Args;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use zp_agent::{Agent, AgentEvent, AgentOptions, AgentTool, AgentToolResult, ToolContext};
use zp_ai::{AssistantMessageEvent, Models, StopReason, StreamOptions, ThinkingLevel, UserContent};

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

#[derive(Clone, Copy)]
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
    eprintln!(
        "{}",
        style.paint(
            "2",
            &format!(
                "{} · thinking {}",
                model.qualified_id(),
                model.clamp_thinking_level(args.thinking)
            )
        )
    );

    let mut options = AgentOptions::with_models(models.clone(), model);
    options.thinking_level = args.thinking;
    options.system_prompt = args.system;
    if args.demo_tool {
        options.tools.push(Arc::new(TimeTool));
    }
    options.request_options = StreamOptions {
        max_tokens: args.max_tokens,
        ..StreamOptions::default()
    };
    let agent = Agent::new(options);
    let printer = Arc::new(Mutex::new(Printer {
        style,
        ends_with_newline: true,
        totals: Totals::default(),
    }));
    let _subscription = {
        let printer = printer.clone();
        agent.subscribe(move |event| {
            if let Ok(mut printer) = printer.lock() {
                printer.handle(event);
            }
        })
    };

    let added = tokio::select! {
        added = agent.prompt(prompt) => added?,
        _ = tokio::signal::ctrl_c() => {
            agent.abort();
            agent.wait_for_idle().await;
            eprintln!("\n{}", style.paint("33", "aborted"));
            std::process::exit(130);
        }
    };

    let totals = printer.lock().map(|p| p.totals).unwrap_or_default();
    eprintln!(
        "{}",
        style.paint(
            "2",
            &format!(
                "tokens: in {} (+{} cached) · out {} · cost ${:.6}",
                totals.input, totals.cached, totals.output, totals.cost
            )
        )
    );
    if let Some(last) = added.iter().rev().find_map(|m| m.as_assistant())
        && matches!(last.stop_reason, StopReason::Error | StopReason::Aborted)
    {
        let error = last.error_message.as_deref().unwrap_or("unknown error");
        eprintln!("{}", style.paint("31", &format!("error: {error}")));
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Totals {
    input: u64,
    cached: u64,
    output: u64,
    cost: f64,
}

/// Prints agent events as they arrive: text to stdout, everything else to stderr.
struct Printer {
    style: Style,
    ends_with_newline: bool,
    totals: Totals,
}

impl Printer {
    fn handle(&mut self, event: &AgentEvent) {
        let mut stdout = std::io::stdout();
        let mut stderr = std::io::stderr();
        match event {
            AgentEvent::MessageUpdate { event } => match event {
                AssistantMessageEvent::ThinkingStart { .. } => {
                    let _ = write!(stderr, "{}", self.style.paint("2;3", "thinking: "));
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    let _ = write!(stderr, "{}", self.style.paint("2;3", delta));
                    let _ = stderr.flush();
                }
                AssistantMessageEvent::ThinkingEnd { .. } => {
                    let _ = writeln!(stderr);
                }
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    self.ends_with_newline = delta.ends_with('\n');
                    let _ = write!(stdout, "{delta}");
                    let _ = stdout.flush();
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message } if message.is_assistant() => {
                if !self.ends_with_newline {
                    let _ = writeln!(stdout);
                    self.ends_with_newline = true;
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                let call = format!("→ {tool_name}({args})");
                let _ = writeln!(stderr, "{}", self.style.paint("36", &call));
            }
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => {
                let output: String = result
                    .content
                    .iter()
                    .filter_map(UserContent::as_text)
                    .collect();
                let color = if *is_error { "31" } else { "36" };
                let _ = writeln!(
                    stderr,
                    "{}",
                    self.style.paint(color, &format!("← {output}"))
                );
            }
            AgentEvent::TurnEnd { message, .. } => {
                let usage = &message.usage;
                self.totals.input += usage.input;
                self.totals.cached += usage.cache_read + usage.cache_write;
                self.totals.output += usage.output;
                self.totals.cost += usage.cost.total;
            }
            _ => {}
        }
    }
}

/// `get_current_time`: the current date and time in UTC.
struct TimeTool;

impl AgentTool for TimeTool {
    fn name(&self) -> &str {
        "get_current_time"
    }

    fn description(&self) -> &str {
        "Get the current date and time in UTC."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}, "required": []})
    }

    fn execute<'a>(
        &'a self,
        _args: Value,
        _ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async { Ok(AgentToolResult::text(current_time_utc())) })
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
