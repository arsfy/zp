//! The coding agent on the command line: one-shot or interactive, with saved sessions.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use tokio::io::AsyncBufReadExt as _;
use zp_agent::{AgentEvent, AgentMessage, AgentToolResult, PromptInput};
use zp_ai::{AssistantMessageEvent, Models, StopReason, StreamOptions, ThinkingLevel, UserContent};
use zp_core::checkpoint::RecordedChange;
use zp_core::permissions::{PermissionGate, PermissionMode};
use zp_core::settings::ProjectSettings;

use crate::approve::{InputLines, TerminalApprover};

fn parse_mode(name: &str) -> Result<PermissionMode, String> {
    PermissionMode::from_cli_name(name).ok_or_else(|| {
        format!("unknown mode `{name}` (use ask, auto-edit, auto-review or full-auto)")
    })
}
use zp_core::{CodingSession, CodingSessionOptions, session_model};
use zp_session::{Session, SessionManager};

#[derive(Args, Clone, Default)]
pub(crate) struct RunArgs {
    /// Project directory (default: the current directory).
    #[arg(short = 'C', long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// Model as `provider/model-id` (default: the session's model, else the first configured
    /// provider's default).
    #[arg(short, long)]
    model: Option<String>,
    /// Reasoning level: off, minimal, low, medium, high, xhigh, max.
    #[arg(short, long)]
    thinking: Option<ThinkingLevel>,
    /// Continue the most recent session of the project. Sessions are not locked: do not
    /// continue the same session in two places (another zp-cli or the app) at once.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["resume", "no_session"])]
    continue_session: bool,
    /// Resume a session by id prefix or file path (see `zp-cli sessions`).
    #[arg(short, long, value_name = "SESSION", conflicts_with = "no_session")]
    resume: Option<String>,
    /// Do not save the conversation.
    #[arg(long)]
    no_session: bool,
    /// Comma-separated tools to enable (default: read,bash,edit,write,grep,find,ls,todo,
    /// and ask when stdin is a terminal).
    #[arg(long, value_delimiter = ',', conflicts_with = "no_tools")]
    tools: Option<Vec<String>>,
    /// Disable all tools.
    #[arg(long)]
    no_tools: bool,
    /// Replace the default system prompt (project context and cwd are still added).
    #[arg(long)]
    system_prompt: Option<String>,
    /// Append text to the system prompt.
    #[arg(long)]
    append_system_prompt: Option<String>,
    /// Output token limit per response.
    #[arg(long)]
    max_tokens: Option<u64>,
    /// Override the model's API base URL (e.g. a proxy or local mock server).
    #[arg(long)]
    base_url: Option<String>,
    /// Interactive mode even when stdin is not a terminal; a prompt argument becomes the
    /// first message.
    #[arg(short, long)]
    interactive: bool,
    /// Permission mode: ask, auto-edit, auto-review or full-auto (default: the settings,
    /// else ask). Approvals are asked on the terminal.
    #[arg(long, value_name = "MODE", value_parser = parse_mode)]
    permissions: Option<PermissionMode>,
    /// Do not start the MCP servers of `~/.zp/mcp.json` / `.zp/mcp.json`.
    #[arg(long)]
    no_mcp: bool,
    /// Trust the project: use its AGENTS.md, `.zp` settings, MCP servers, prompts and skills.
    /// Remembered, like trusting it in the app. Without it, ZP asks on a terminal and
    /// otherwise ignores them.
    #[arg(long)]
    trust: bool,
    /// The prompt. Read from stdin when piped; omit on a terminal for interactive mode.
    /// `@path` attaches a file, `$name` or `/skill:name` loads a skill, and `/template args` expands a template.
    prompt: Vec<String>,
}

#[derive(Args)]
pub(crate) struct SessionsArgs {
    /// Project directory (default: the current directory).
    #[arg(short = 'C', long, value_name = "DIR")]
    cwd: Option<PathBuf>,
    /// List the sessions of every project.
    #[arg(short, long)]
    all: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Style {
    enabled: bool,
}

impl Style {
    pub(crate) fn detect() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
        }
    }

    pub(crate) fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// The project directory in the same canonical form the GUI uses, so both find the same
/// sessions.
fn project_dir(cwd: Option<&Path>) -> Result<PathBuf> {
    let dir = match cwd {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir()?,
    };
    zp_core::project::normalize_project_path(&dir)
        .with_context(|| format!("{} is not a usable project directory", dir.display()))
}

fn session_manager() -> Result<(PathBuf, SessionManager)> {
    let data_dir = zp_core::paths::data_dir()?;
    let manager = SessionManager::new(zp_core::paths::sessions_dir(&data_dir));
    Ok((data_dir, manager))
}

pub(crate) async fn run(models: &Models, args: RunArgs) -> Result<()> {
    let cwd = project_dir(args.cwd.as_deref())?;
    let (data_dir, manager) = session_manager()?;
    let session = if args.no_session {
        Session::in_memory(&cwd)
    } else if let Some(id) = &args.resume {
        let info = manager
            .find(&cwd, id)?
            .with_context(|| format!("no session `{id}` for {}", cwd.display()))?;
        manager.open(&info.path)?
    } else if args.continue_session {
        manager.continue_recent(&cwd)?
    } else {
        manager.create(&cwd)
    };

    let prompt = if !args.prompt.is_empty() {
        Some(args.prompt.join(" "))
    } else if !std::io::stdin().is_terminal() && !args.interactive {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        Some(input)
    } else {
        None
    };
    if prompt.as_deref().is_some_and(|p| p.trim().is_empty()) {
        bail!("the prompt is empty");
    }

    let mut model = match &args.model {
        Some(spec) => Some(crate::select_model(models, Some(spec))?),
        None if session_model(models, &session).is_some() => None,
        None => Some(crate::select_model(models, None)?),
    };
    if let Some(base_url) = &args.base_url {
        let mut chosen = model
            .or_else(|| session_model(models, &session))
            .context("no model to apply --base-url to")?;
        chosen.base_url.clone_from(base_url);
        model = Some(chosen);
    }
    // `ask` is added below when there is a terminal to answer on.
    let mut ask = !args.no_tools;
    let mut delegate = !args.no_tools && args.tools.is_none();
    // `subagent` named in `--tools`, so the user expects it.
    let mut delegate_requested = false;
    let tools = if args.no_tools {
        Some(Vec::new())
    } else if let Some(names) = &args.tools {
        let mut tools = Vec::new();
        ask = false;
        for name in names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
            if name == "ask" {
                ask = true;
                continue;
            }
            if name == zp_core::subagents::TOOL_NAME {
                delegate = true;
                delegate_requested = true;
                continue;
            }
            tools.push(zp_tools::create_tool(name, &cwd).with_context(|| {
                format!(
                    "unknown tool `{name}` (available: {}, ask, subagent)",
                    zp_tools::ALL_TOOLS.join(", ")
                )
            })?);
        }
        Some(tools)
    } else {
        None
    };

    // New sessions start at the default reasoning level of the settings.
    let default_thinking = session
        .context()
        .thinking_level
        .is_none()
        .then(|| {
            zp_core::settings::Settings::load_or_default(&zp_core::paths::settings_file(&data_dir))
                .default_thinking
        })
        .flatten();
    let mut options = CodingSessionOptions::new(models.clone(), &cwd, session);
    options.model = model;
    options.thinking_level = args.thinking.or(default_thinking);
    options.tools = tools;
    if !args.no_session {
        options.blobs = Some(zp_core::checkpoint::BlobStore::in_data_dir(&data_dir));
    }
    // Only once the user said so (here or in the app): a cloned repository must not get to
    // change the permissions, instruct the agent or start its MCP servers by itself.
    let trusted = crate::trust::resolve(Some(&data_dir), &cwd, args.trust);
    let settings = ProjectSettings::load(Some(&data_dir), &cwd, trusted);
    if delegate {
        options.subagents = settings.subagents();
        if delegate_requested && !options.subagents.is_usable() {
            eprintln!(
                "warning: the subagent tool is not available: enable subagents (and add a task \
                 profile when using task profiles) in the settings"
            );
        }
    }
    let policy = settings.permission_policy(&cwd, args.permissions);
    // Approvals read the same stdin as the interactive prompt.
    let input: Option<InputLines> = std::io::stdin().is_terminal().then(|| {
        Arc::new(tokio::sync::Mutex::new(
            tokio::io::BufReader::new(tokio::io::stdin()).lines(),
        ))
    });
    let approver = Arc::new(TerminalApprover::new(input.clone(), Style::detect()));
    if ask && let Some(input) = &input {
        options.asker = Some(Arc::new(crate::ask::TerminalAsker::new(
            input.clone(),
            Style::detect(),
        )));
    }
    options.permissions = Some(Arc::new(PermissionGate::new(
        policy,
        approver,
        models.clone(),
    )));
    options.trusted = trusted;
    options.resources = Arc::new(zp_core::resources::Resources::load(
        Some(&data_dir),
        &cwd,
        trusted,
    ));
    let user_settings =
        zp_core::settings::Settings::load_or_default(&zp_core::paths::settings_file(&data_dir));
    options.compaction = user_settings.compaction.unwrap_or_default();
    // MCP servers add their tools to the built-in ones (not with an explicit tool list).
    let mcp = if options.tools.is_none() && !args.no_mcp {
        let env = |name: &str| std::env::var(name).ok();
        let (servers, errors) = zp_mcp::config::load(Some(&data_dir), &cwd, trusted, &env);
        for error in &errors {
            eprintln!("{}", Style::detect().paint("33", &format!("mcp: {error}")));
        }
        (!servers.is_empty()).then(|| zp_mcp::McpHub::new(servers, errors))
    } else {
        None
    };
    if let Some(hub) = &mcp {
        hub.start();
        wait_for_servers(hub).await;
        for status in hub.statuses() {
            if let zp_mcp::ServerState::Failed(error) = &status.state {
                eprintln!(
                    "{}",
                    Style::detect().paint("33", &format!("mcp: {}: {error}", status.name))
                );
            }
        }
        let mut tools = zp_tools::coding_tools(&cwd);
        tools.extend(hub.tools());
        options.tools = Some(tools);
    }
    options.global_context_dir = Some(data_dir);
    options.custom_system_prompt = args.system_prompt;
    options.append_system_prompt = args.append_system_prompt;
    options.request_options = StreamOptions {
        max_tokens: args.max_tokens,
        cache_retention: user_settings.cache_retention.unwrap_or_default(),
        ..StreamOptions::default()
    };
    let coding = CodingSession::new(options)?;
    let style = Style::detect();
    print_header(&coding, style);

    let printer = Arc::new(Mutex::new(Printer::new(style)));
    let _subscription = {
        let printer = printer.clone();
        coding.agent().subscribe(move |event| {
            if let Ok(mut printer) = printer.lock() {
                printer.handle(event);
            }
        })
    };

    match prompt {
        Some(prompt) if args.interactive => {
            if let Err(error) = run_prompt(&coding, &prompt).await {
                eprintln!("{}", style.paint("31", &format!("error: {error:#}")));
            }
            print_usage(&coding, style);
            interactive(&coding, models, style, input).await
        }
        Some(prompt) => {
            let added = run_prompt(&coding, &prompt).await?;
            print_usage(&coding, style);
            let failed = added
                .iter()
                .rev()
                .find_map(AgentMessage::as_assistant)
                .is_some_and(|m| matches!(m.stop_reason, StopReason::Error | StopReason::Aborted));
            if failed {
                // `exit` skips destructors: stop the MCP servers (their process trees) here.
                if let Some(hub) = &mcp {
                    hub.shutdown();
                }
                std::process::exit(1);
            }
            Ok(())
        }
        None => interactive(&coding, models, style, input).await,
    }
}

fn print_header(coding: &CodingSession, style: Style) {
    let agent = coding.agent();
    let session = coding.session();
    let (model, thinking) = match agent.model() {
        Some(model) => (
            model.qualified_id(),
            model.clamp_thinking_level(agent.thinking_level()),
        ),
        None => ("no model".to_owned(), agent.thinking_level()),
    };
    eprintln!(
        "{}",
        style.paint(
            "2",
            &format!("{model} · thinking {thinking} · {}", coding.cwd().display())
        )
    );
    let saved = match session.file() {
        Some(path) => format!("session {}", path.display()),
        None => "session not saved".to_owned(),
    };
    let restored = agent.messages().len();
    let restored = if restored > 0 {
        format!(" · resumed {restored} messages")
    } else {
        String::new()
    };
    eprintln!("{}", style.paint("2", &format!("{saved}{restored}")));
}

fn print_usage(coding: &CodingSession, style: Style) {
    let usage = coding.usage();
    let window = coding
        .agent()
        .model()
        .map_or(0, |model| model.context_window);
    let cache = coding
        .last_cache_read_rate()
        .map_or_else(String::new, |rate| format!(" · cache {:.1}%", rate * 100.0));
    let context = if window > 0 && usage.last_context_tokens > 0 {
        format!(
            " · context {:.0}%",
            usage.last_context_tokens as f64 * 100.0 / window as f64
        )
    } else {
        String::new()
    };
    eprintln!(
        "{}",
        style.paint(
            "2",
            &format!(
                "tokens: in {} (+{} read, +{} written) · out {} · cost ${:.4}{cache}{context}",
                usage.input, usage.cache_read, usage.cache_write, usage.output, usage.cost
            )
        )
    );
}

/// Waits up to 10 seconds for MCP servers to connect.
async fn wait_for_servers(hub: &Arc<zp_mcp::McpHub>) {
    let mut changes = hub.subscribe();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while hub
        .statuses()
        .iter()
        .any(|s| s.state == zp_mcp::ServerState::Connecting)
    {
        if tokio::time::timeout_at(deadline, changes.changed())
            .await
            .is_err()
        {
            break;
        }
    }
}

/// Runs one prompt (templates and skills expanded, `@file`s attached, the context
/// compacted when needed); Ctrl-C aborts it. A request too large for the context is
/// compacted and retried once.
async fn run_prompt(coding: &CodingSession, input: &str) -> Result<Vec<AgentMessage>> {
    let style = Style::detect();
    let text = coding.expand_input(input);
    let (message, notes) = zp_core::attachments::build_message(&text, coding.cwd(), &[]);
    for note in notes {
        eprintln!("{}", style.paint("33", &note));
    }
    abortable(coding, coding.prompt(PromptInput(vec![message]))).await
}

async fn abortable(
    coding: &CodingSession,
    run: impl std::future::Future<Output = Result<Vec<AgentMessage>, zp_agent::AgentError>>,
) -> Result<Vec<AgentMessage>> {
    tokio::pin!(run);
    loop {
        tokio::select! {
            result = &mut run => return Ok(result?),
            _ = tokio::signal::ctrl_c() => coding.agent().abort(),
        }
    }
}

/// Summarizes older messages.
async fn compact(coding: &CodingSession, focus: Option<&str>) -> Result<()> {
    let style = Style::detect();
    eprintln!("{}", style.paint("2", "compacting…"));
    let Some((before, after)) = coding.compact(focus).await? else {
        eprintln!("{}", style.paint("2", "nothing to compact yet"));
        return Ok(());
    };
    eprintln!(
        "{}",
        style.paint("2", &format!("compacted: ~{before} → ~{after} tokens"))
    );
    Ok(())
}

async fn interactive(
    coding: &CodingSession,
    models: &Models,
    style: Style,
    input: Option<InputLines>,
) -> Result<()> {
    eprintln!(
        "{}",
        style.paint("2", "Type a message (end a line with \\ to continue it), /help for commands, Ctrl-D to quit.")
    );
    let stdin_lines = input.unwrap_or_else(|| {
        Arc::new(tokio::sync::Mutex::new(
            tokio::io::BufReader::new(tokio::io::stdin()).lines(),
        ))
    });
    'session: loop {
        let mut input = String::new();
        let mut prompt_marker = "› ";
        loop {
            eprint!("{}", style.paint("1;32", prompt_marker));
            let _ = std::io::stderr().flush();
            let line = tokio::select! {
                line = async { stdin_lines.lock().await.next_line().await } => line?,
                _ = tokio::signal::ctrl_c() => {
                    eprintln!();
                    break 'session;
                }
            };
            let Some(line) = line else {
                eprintln!();
                break 'session;
            };
            match line.strip_suffix('\\') {
                Some(partial) => {
                    input.push_str(partial);
                    input.push('\n');
                    prompt_marker = "… ";
                }
                None => {
                    input.push_str(&line);
                    break;
                }
            }
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if let Some(focus) = input.strip_prefix("/compact") {
            if let Err(error) = compact(coding, Some(focus.trim()).filter(|f| !f.is_empty())).await
            {
                eprintln!("{}", style.paint("31", &format!("error: {error:#}")));
            }
            continue;
        }
        let expands = coding.resources().expand(input).is_some();
        if let Some(command) = input.strip_prefix('/').filter(|_| !expands) {
            match command_line(coding, models, command, style) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(error) => {
                    eprintln!("{}", style.paint("31", &format!("error: {error:#}")));
                    continue;
                }
            }
        }
        if let Err(error) = run_prompt(coding, input).await {
            eprintln!("{}", style.paint("31", &format!("error: {error:#}")));
        }
        print_usage(coding, style);
    }
    let (file, id) = {
        let session = coding.session();
        (
            session.file().map(Path::to_path_buf),
            session.id().to_owned(),
        )
    };
    if let Some(path) = file.filter(|p| p.exists()) {
        eprintln!("{}", style.paint("2", &format!("saved {}", path.display())));
        eprintln!(
            "{}",
            style.paint("2", &format!("resume with: zp-cli -r {}", &id[..13]))
        );
    }
    Ok(())
}

/// Handles `/command`; returns `false` to quit.
fn command_line(
    coding: &CodingSession,
    models: &Models,
    command: &str,
    style: Style,
) -> Result<bool> {
    let (name, argument) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(n, a)| (n, a.trim()));
    match name {
        "exit" | "quit" | "q" => return Ok(false),
        "help" => eprintln!(
            "/model [provider/id]   show or switch the model\n\
             /thinking [level]      show or set the reasoning level\n\
             /name <text>           name this session\n\
             /session               show session details\n\
             /tools                 list enabled tools\n\
             /compact [focus]       summarize older messages\n\
             /<template> [args]     run a prompt template\n\
             $<name> or /skill:<name>  load a skill\n\
             /exit                  quit"
        ),
        "model" if argument.is_empty() => eprintln!(
            "{}",
            coding
                .agent()
                .model()
                .map_or_else(|| "no model".to_owned(), |model| model.qualified_id())
        ),
        "model" => {
            let model = crate::select_model(models, Some(argument))?;
            eprintln!(
                "{}",
                style.paint("2", &format!("model: {}", model.qualified_id()))
            );
            coding.set_model(model)?;
        }
        "thinking" if argument.is_empty() => eprintln!("{}", coding.agent().thinking_level()),
        "thinking" => {
            let level: ThinkingLevel = argument.parse().map_err(anyhow::Error::msg)?;
            coding.set_thinking_level(level)?;
            let effective = coding
                .agent()
                .model()
                .map_or(level, |model| model.clamp_thinking_level(level));
            eprintln!("{}", style.paint("2", &format!("thinking: {effective}")));
        }
        "name" if !argument.is_empty() => coding.set_name(Some(argument.to_owned()))?,
        "session" => {
            let session = coding.session();
            eprintln!("id:       {}", session.id());
            match session.file() {
                Some(path) => eprintln!("file:     {}", path.display()),
                None => eprintln!("file:     (not saved)"),
            }
            eprintln!("name:     {}", session.name().unwrap_or("-"));
            eprintln!("entries:  {}", session.entries().len());
            eprintln!("messages: {}", coding.agent().messages().len());
        }
        "tools" => {
            let names: Vec<String> = coding
                .agent()
                .tools()
                .iter()
                .map(|t| t.name().to_owned())
                .collect();
            eprintln!(
                "{}",
                if names.is_empty() {
                    "(none)".into()
                } else {
                    names.join(", ")
                }
            );
        }
        other => bail!("unknown command /{other} (try /help)"),
    }
    Ok(true)
}

pub(crate) fn list_sessions(args: SessionsArgs) -> Result<()> {
    let (_, manager) = session_manager()?;
    let sessions = if args.all {
        manager.list_all()?
    } else {
        manager.list(&project_dir(args.cwd.as_deref())?)?
    };
    if sessions.is_empty() {
        eprintln!("no sessions");
        return Ok(());
    }
    let mut rows = vec![[
        "ID".to_owned(),
        "UPDATED (UTC)".to_owned(),
        "MSGS".to_owned(),
        "TITLE".to_owned(),
    ]];
    for info in &sessions {
        let mut title: String = info.title().chars().take(60).collect();
        if args.all {
            title = format!("{title}  [{}]", info.cwd);
        }
        rows.push([
            info.id.chars().take(13).collect(),
            zp_session::time::format_iso(info.modified)[..16].replace('T', " "),
            info.message_count.to_string(),
            title,
        ]);
    }
    crate::print_table(&rows);
    Ok(())
}

// ----- event printing --------------------------------------------------------------------

/// Prints agent events as they arrive: answer text to stdout, everything else to stderr.
struct Printer {
    style: Style,
    ends_with_newline: bool,
}

impl Printer {
    fn new(style: Style) -> Self {
        Self {
            style,
            ends_with_newline: true,
        }
    }

    fn handle(&mut self, event: &AgentEvent) {
        let mut stdout = std::io::stdout();
        let mut stderr = std::io::stderr();
        match event {
            AgentEvent::ContextCompacting { .. } => {
                let _ = writeln!(stderr, "{}", self.style.paint("2", "compacting…"));
            }
            AgentEvent::ContextCompacted {
                compaction,
                trigger,
            } => {
                let notice = format!(
                    "compacted: ~{} → ~{} tokens",
                    compaction.tokens_before, compaction.tokens_after
                );
                let _ = writeln!(stderr, "{}", self.style.paint("2", &notice));
                if *trigger == zp_agent::CompactionTrigger::ProviderError {
                    let _ = writeln!(
                        stderr,
                        "{}",
                        self.style.paint("2", "retrying after context overflow")
                    );
                }
            }
            AgentEvent::ContextCompactionFailed { error } => {
                let _ = writeln!(stderr, "{}", self.style.paint("33", error));
            }
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
            AgentEvent::MessageEnd { message } => {
                let Some(message) = message.as_assistant() else {
                    return;
                };
                if !self.ends_with_newline {
                    let _ = writeln!(stdout);
                    self.ends_with_newline = true;
                }
                if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                    let error = message.error_message.as_deref().unwrap_or("request failed");
                    let _ = writeln!(
                        stderr,
                        "{}",
                        self.style.paint("31", &format!("error: {error}"))
                    );
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                let call = format!(
                    "→ {tool_name} {}",
                    zp_tools::describe_call(tool_name, args, 120)
                );
                let _ = writeln!(stderr, "{}", self.style.paint("36", call.trim_end()));
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } => {
                let summary = summarize_result(tool_name, result, *is_error);
                if !summary.is_empty() {
                    let color = if *is_error { "31" } else { "2" };
                    let _ = writeln!(stderr, "{}", self.style.paint(color, &summary));
                }
            }
            _ => {}
        }
    }
}

fn indent(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn summarize_result(tool: &str, result: &AgentToolResult, is_error: bool) -> String {
    const PREVIEW_LINES: usize = 8;
    let text: String = result
        .content
        .iter()
        .filter_map(UserContent::as_text)
        .collect();
    let lines: Vec<&str> = text.lines().collect();
    if is_error {
        let shown = &lines[lines.len().saturating_sub(PREVIEW_LINES)..];
        return indent(shown);
    }
    if let Some(change) = result
        .details
        .as_ref()
        .and_then(RecordedChange::from_details)
    {
        return format!("  +{} -{}", change.added, change.removed);
    }
    match tool {
        "read" => format!("  {} lines", lines.len()),
        // The whole list: it is the progress report.
        "todo" => result
            .details
            .as_ref()
            .and_then(zp_tools::TodoList::from_details)
            .map(|list| indent(&list.render().lines().collect::<Vec<_>>()))
            .unwrap_or_default(),
        "bash" => {
            let mut shown = indent(&lines[lines.len().saturating_sub(PREVIEW_LINES)..]);
            if lines.len() > PREVIEW_LINES {
                shown = format!("  … {} earlier lines\n{shown}", lines.len() - PREVIEW_LINES);
            }
            shown
        }
        _ => {
            let mut shown = indent(&lines[..lines.len().min(PREVIEW_LINES)]);
            if lines.len() > PREVIEW_LINES {
                shown.push_str(&format!("\n  … {} more lines", lines.len() - PREVIEW_LINES));
            }
            shown
        }
    }
}
