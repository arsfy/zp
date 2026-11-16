//! A minimal MCP client: JSON-RPC over stdio (newline-delimited) or streamable HTTP
//! (JSON or SSE responses), with `initialize`, `tools/list` and `tools/call`.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config::{Server, Transport};

/// The protocol revision ZP speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Lines of a stdio server's stderr kept for error messages.
const STDERR_LINES: usize = 20;

/// A tool offered by a server.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "empty_schema")]
    pub input_schema: Value,
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

enum Connection {
    Stdio {
        stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
        child: Box<Mutex<Option<tokio::process::Child>>>,
        stderr: Arc<Mutex<VecDeque<String>>>,
    },
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        session: Mutex<Option<String>>,
    },
}

/// A connected server.
pub struct Client {
    connection: Connection,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    /// Set when the server announced a changed tool list.
    pub tools_changed: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    /// `instructions` from the server's `initialize` result.
    pub instructions: Option<String>,
}

fn rpc_error(error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("unknown error");
    match error["code"].as_i64() {
        Some(code) => format!("{message} (code {code})"),
        None => message.to_owned(),
    }
}

/// Routes one message from the server: responses to their callers; requests answered.
fn route(message: Value, pending: &Pending, tools_changed: &AtomicBool) -> Option<Value> {
    let id = message.get("id").cloned();
    match (message.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => {
            // A request from the server.
            Some(match method {
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                _ => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported"}})
                }
            })
        }
        (Some(method), None) => {
            if method == "notifications/tools/list_changed" {
                tools_changed.store(true, Ordering::SeqCst);
            }
            None
        }
        (None, Some(id)) => {
            let id = id.as_u64()?;
            let sender = pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id)?;
            let result = match message.get("error") {
                Some(error) => Err(rpc_error(error)),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(result);
            None
        }
        (None, None) => None,
    }
}

impl Client {
    /// Starts or reaches `server` and completes the `initialize` handshake.
    pub async fn connect(server: &Server) -> anyhow::Result<Self> {
        let pending: Pending = Arc::default();
        let tools_changed = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let connection = match &server.transport {
            Transport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut process = tokio::process::Command::new(command);
                process
                    .args(args)
                    .envs(env)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                if let Some(cwd) = cwd {
                    process.current_dir(cwd);
                }
                #[cfg(unix)]
                process.process_group(0);
                let mut child = process
                    .spawn()
                    .with_context(|| format!("could not start `{command}`"))?;
                let stdin = child.stdin.take().context("no stdin")?;
                let stdout = child.stdout.take().context("no stdout")?;
                let stderr_pipe = child.stderr.take().context("no stderr")?;
                let stderr: Arc<Mutex<VecDeque<String>>> = Arc::default();
                {
                    let stderr = stderr.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stderr_pipe).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let mut tail = stderr.lock().unwrap_or_else(PoisonError::into_inner);
                            if tail.len() == STDERR_LINES {
                                tail.pop_front();
                            }
                            tail.push_back(line);
                        }
                    });
                }
                let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
                {
                    let pending = pending.clone();
                    let tools_changed = tools_changed.clone();
                    let closed = closed.clone();
                    let stdin = stdin.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            // Answer server requests (`ping`) right away.
                            if let Some(reply) = route(message, &pending, &tools_changed)
                                && let Ok(mut text) = serde_json::to_string(&reply)
                            {
                                text.push('\n');
                                let mut stdin = stdin.lock().await;
                                let _ = stdin.write_all(text.as_bytes()).await;
                                let _ = stdin.flush().await;
                            }
                        }
                        closed.store(true, Ordering::SeqCst);
                        for (_, sender) in pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .drain()
                        {
                            let _ = sender.send(Err("the server exited".into()));
                        }
                    });
                }
                Connection::Stdio {
                    stdin,
                    child: Box::new(Mutex::new(Some(child))),
                    stderr,
                }
            }
            Transport::Http { url, headers } => Connection::Http {
                http: reqwest::Client::new(),
                url: url.clone(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                session: Mutex::new(None),
            },
        };
        let mut client = Self {
            connection,
            pending,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(server.timeout_secs),
            tools_changed,
            closed,
            instructions: None,
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&mut self) -> anyhow::Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "zp", "version": env!("CARGO_PKG_VERSION")},
                }),
                None,
            )
            .await
            .map_err(|error| self.with_stderr(error))?;
        self.instructions = result["instructions"].as_str().map(str::to_owned);
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// The error with the tail of a stdio server's stderr.
    fn with_stderr(&self, error: anyhow::Error) -> anyhow::Error {
        if let Connection::Stdio { stderr, .. } = &self.connection {
            let tail: Vec<String> = stderr
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .cloned()
                .collect();
            if !tail.is_empty() {
                return error.context(format!("server output:\n{}", tail.join("\n")));
            }
        }
        error
    }

    /// Whether the connection is gone (a stdio server exited).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn send(&self, message: &Value, id: Option<u64>) -> anyhow::Result<Option<Value>> {
        match &self.connection {
            Connection::Stdio { stdin, .. } => {
                let mut line = serde_json::to_string(message)?;
                line.push('\n');
                let mut stdin = stdin.lock().await;
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .context("the server closed its input")?;
                stdin.flush().await?;
                Ok(None)
            }
            Connection::Http {
                http,
                url,
                headers,
                session,
            } => {
                let mut request = http
                    .post(url)
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header("mcp-protocol-version", PROTOCOL_VERSION)
                    .body(serde_json::to_string(message)?);
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let current = session
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                if let Some(current) = current {
                    request = request.header("mcp-session-id", current);
                }
                let response = request.send().await.context("could not reach the server")?;
                if let Some(id) = response
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                {
                    *session.lock().unwrap_or_else(PoisonError::into_inner) = Some(id.to_owned());
                }
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    bail!(
                        "HTTP {status}: {}",
                        body.chars().take(300).collect::<String>()
                    );
                }
                let Some(id) = id else {
                    return Ok(None);
                };
                let event_stream = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("text/event-stream"));
                if !event_stream {
                    let text = response
                        .text()
                        .await
                        .context("could not read the response")?;
                    let body: Value =
                        serde_json::from_str(&text).context("the server sent invalid JSON")?;
                    let messages = if body.is_array() {
                        body.as_array().cloned().unwrap_or_default()
                    } else {
                        vec![body]
                    };
                    return Ok(messages.into_iter().find(|m| m["id"].as_u64() == Some(id)));
                }
                // Server-sent events: wait for the response with our id.
                let mut stream = response.bytes_stream();
                let mut buffer = String::new();
                while let Some(chunk) = stream.next().await {
                    buffer.push_str(&String::from_utf8_lossy(&chunk?));
                    while let Some(end) = buffer.find("\n\n") {
                        let event: String = buffer.drain(..end + 2).collect();
                        let data: String = event
                            .lines()
                            .filter_map(|line| line.strip_prefix("data:"))
                            .map(str::trim_start)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let Ok(message) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        if message["id"].as_u64() == Some(id) && message.get("method").is_none() {
                            return Ok(Some(message));
                        }
                        if message["method"] == "notifications/tools/list_changed" {
                            self.tools_changed.store(true, Ordering::SeqCst);
                        }
                    }
                }
                bail!("the server closed the stream without answering")
            }
        }
    }

    /// Sends a request and waits for its result.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&CancellationToken>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        let exchange = async {
            match self.send(&message, Some(id)).await? {
                // HTTP: the answer came with the response.
                Some(response) => match response.get("error") {
                    Some(error) => Err(anyhow!(rpc_error(error))),
                    None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
                },
                None => match receiver.await {
                    Ok(result) => result.map_err(|e| anyhow!(e)),
                    Err(_) => Err(anyhow!("the connection closed")),
                },
            }
        };
        let never = CancellationToken::new();
        let cancel = cancel.unwrap_or(&never);
        let outcome = tokio::select! {
            result = tokio::time::timeout(self.timeout, exchange) => {
                result.unwrap_or_else(|_| Err(anyhow!("no answer within {} s", self.timeout.as_secs())))
            }
            () = cancel.cancelled() => {
                let _ = self
                    .notify("notifications/cancelled", json!({"requestId": id, "reason": "cancelled by the user"}))
                    .await;
                Err(anyhow!("cancelled"))
            }
        };
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        outcome.with_context(|| format!("`{method}` failed"))
    }

    pub async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Every tool of the server (following pagination).
    pub async fn list_tools(&self) -> anyhow::Result<Vec<ToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("tools/list", params, None).await?;
            let page: Vec<ToolInfo> = serde_json::from_value(result["tools"].clone())
                .context("the server sent an invalid tool list")?;
            tools.extend(page);
            cursor = result["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        self.tools_changed.store(false, Ordering::SeqCst);
        Ok(tools)
    }

    /// Calls tool `name`; returns the raw `tools/call` result.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Value> {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            Some(cancel),
        )
        .await
    }

    /// Stops a stdio server (its whole process group).
    pub fn shutdown(&self) {
        if let Connection::Stdio { child, .. } = &self.connection
            && let Some(mut child) = child.lock().unwrap_or_else(PoisonError::into_inner).take()
        {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                zp_tools::kill_tree(pid);
            }
            let _ = child.start_kill();
        }
        self.closed.store(true, Ordering::SeqCst);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}
