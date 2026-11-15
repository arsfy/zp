//! `bash`: run a shell command with streaming output, timeout and cancellation.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio::time::Instant;
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

use crate::output::OutputBuffer;
use crate::shell::{Shell, kill_tree};
use crate::truncate::{DEFAULT_MAX_BYTES, TruncatedBy, format_size};

/// Minimum spacing of progress updates.
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);
/// How long to keep reading after the shell exits, for output still in the pipes. Background
/// processes that keep the pipes open do not hold the call beyond this.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Runs shell commands in a working directory.
#[derive(Clone, Debug)]
pub struct BashTool {
    cwd: PathBuf,
    shell: Shell,
}

impl BashTool {
    /// A bash tool for `cwd`, using the detected shell.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_shell(cwd, Shell::detect())
    }

    /// A bash tool using `shell`.
    pub fn with_shell(cwd: impl Into<PathBuf>, shell: Shell) -> Self {
        Self {
            cwd: cwd.into(),
            shell,
        }
    }
}

enum Outcome {
    Exited(ExitStatus),
    Aborted,
    TimedOut(f64),
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_owned()
    } else {
        format!("{text}\n\n{status}")
    }
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. \
         Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, \
         full output is saved to a temp file. Optionally provide a timeout in seconds."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Execute bash commands (build, test, git, etc.)")
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"}
            },
            "required": ["command"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let command = args["command"].as_str().unwrap_or_default();
            let timeout = match args["timeout"].as_f64() {
                Some(t) if !t.is_finite() || t <= 0.0 || t > 86_400.0 * 30.0 => {
                    bail!("Invalid timeout: must be a positive number of seconds")
                }
                other => other,
            };
            if !self.cwd.is_dir() {
                bail!(
                    "Working directory does not exist: {}\nCannot execute bash commands.",
                    self.cwd.display()
                );
            }

            let mut cmd = tokio::process::Command::new(&self.shell.program);
            cmd.args(&self.shell.args)
                .arg(command)
                .current_dir(&self.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env("PAGER", "cat")
                .env("GIT_PAGER", "cat")
                .env("GIT_TERMINAL_PROMPT", "0")
                .kill_on_drop(true);
            #[cfg(unix)]
            cmd.process_group(0);
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            let mut child = cmd
                .spawn()
                .with_context(|| format!("Failed to start {}", self.shell.program.display()))?;
            let pid = child.id();

            let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<u8>>();
            let mut readers = Vec::new();
            macro_rules! forward {
                ($pipe:expr) => {
                    if let Some(mut pipe) = $pipe {
                        let sender = sender.clone();
                        readers.push(tokio::spawn(async move {
                            let mut buffer = vec![0u8; 8192];
                            while let Ok(n) = pipe.read(&mut buffer).await {
                                if n == 0 || sender.send(buffer[..n].to_vec()).is_err() {
                                    break;
                                }
                            }
                        }));
                    }
                };
            }
            forward!(child.stdout.take());
            forward!(child.stderr.take());
            drop(sender);

            let mut output = OutputBuffer::new();
            let mut last_update = Instant::now();
            let mut dirty = false;
            let mut readers_done = false;
            let deadline = timeout.map(|t| Instant::now() + Duration::from_secs_f64(t));
            let kill = || {
                if let Some(pid) = pid {
                    kill_tree(pid);
                }
            };

            let outcome = loop {
                tokio::select! {
                    () = ctx.cancel.cancelled() => {
                        kill();
                        break Outcome::Aborted;
                    }
                    () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                        kill();
                        break Outcome::TimedOut(timeout.unwrap_or_default());
                    }
                    chunk = receiver.recv(), if !readers_done => match chunk {
                        Some(chunk) => {
                            output.push(&chunk);
                            dirty = true;
                        }
                        None => readers_done = true,
                    },
                    status = child.wait() => {
                        break Outcome::Exited(status.context("Failed to wait for the command")?);
                    }
                    () = tokio::time::sleep_until(last_update + UPDATE_INTERVAL), if dirty => {}
                }
                if dirty && last_update.elapsed() >= UPDATE_INTERVAL {
                    dirty = false;
                    last_update = Instant::now();
                    ctx.update(AgentToolResult::text(output.snapshot(false).0.content));
                }
            };

            // Collect what is still in the pipes, without waiting on background processes.
            let grace = Instant::now() + DRAIN_GRACE;
            while !readers_done {
                tokio::select! {
                    chunk = receiver.recv() => match chunk {
                        Some(chunk) => output.push(&chunk),
                        None => readers_done = true,
                    },
                    () = tokio::time::sleep_until(grace) => break,
                }
            }
            for reader in readers {
                reader.abort();
            }
            if !matches!(outcome, Outcome::Exited(_)) {
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }

            let empty = output.is_empty();
            let (cut, full_path) = output.snapshot(true);
            let mut text = cut.content.trim_end_matches('\n').to_owned();
            let mut details = serde_json::Map::new();
            if cut.info.truncated || full_path.is_some() {
                let full = full_path
                    .as_ref()
                    .map_or_else(|| "unavailable".to_owned(), |p| p.display().to_string());
                let end = cut.info.total_lines;
                let start = end + 1 - cut.info.output_lines.max(1);
                let notice = if cut.info.last_line_partial {
                    format!(
                        "[Showing last {} of line {end}. Full output: {full}]",
                        format_size(cut.info.output_bytes)
                    )
                } else if cut.info.truncated_by == Some(TruncatedBy::Bytes) {
                    format!(
                        "[Showing lines {start}-{end} of {end} ({} limit). Full output: {full}]",
                        format_size(DEFAULT_MAX_BYTES)
                    )
                } else {
                    format!("[Showing lines {start}-{end} of {end}. Full output: {full}]")
                };
                text = append_status(&text, &notice);
                details.insert("truncation".into(), json!(cut.info));
                if let Some(path) = &full_path {
                    details.insert("fullOutputPath".into(), json!(path));
                }
            }

            match outcome {
                Outcome::Aborted => Err(anyhow!(append_status(&text, "Command aborted"))),
                Outcome::TimedOut(seconds) => Err(anyhow!(append_status(
                    &text,
                    &format!("Command timed out after {seconds} seconds")
                ))),
                Outcome::Exited(status) => {
                    let code = exit_code(status);
                    details.insert("exitCode".into(), json!(code));
                    if empty || text.is_empty() {
                        text = "(no output)".into();
                    }
                    let result = if code == 0 {
                        AgentToolResult::text(text)
                    } else {
                        AgentToolResult::error(append_status(
                            &text,
                            &format!("Command exited with code {code}"),
                        ))
                    };
                    Ok(result.with_details(Value::Object(details)))
                }
            }
        })
    }
}
