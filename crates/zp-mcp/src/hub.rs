//! The MCP servers of a project: connecting them in the background and exposing their
//! tools to the agent as `mcp__<server>__<tool>`.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::watch;
use zp_agent::{AgentTool, AgentToolResult, ToolContext};
use zp_ai::{ImageContent, UserContent};

use crate::client::{Client, ToolInfo};
use crate::config::Server;

/// Longest tool name most providers accept.
const MAX_TOOL_NAME: usize = 64;

/// Connection state of a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerState {
    Disabled,
    Connecting,
    Connected,
    Failed(String),
}

/// A server as shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub state: ServerState,
    /// Tool names as the server calls them.
    pub tools: Vec<String>,
    pub description: Option<String>,
    pub source: std::path::PathBuf,
}

struct Entry {
    server: Server,
    state: ServerState,
    client: Option<Arc<Client>>,
    tools: Vec<ToolInfo>,
}

/// The model-facing name of `tool` on `server`: `mcp__server__tool` with characters
/// other than letters, digits and `_` replaced, shortened with a hash when too long.
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    if name.len() <= MAX_TOOL_NAME {
        return name;
    }
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{}_{:08x}", &name[..MAX_TOOL_NAME - 9], hash as u32)
}

/// Converts a `tools/call` result into a tool result.
pub fn convert_result(result: &Value) -> AgentToolResult {
    let mut content = Vec::new();
    for block in result["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => content.push(UserContent::text(
                block["text"].as_str().unwrap_or_default(),
            )),
            Some("image") => content.push(UserContent::Image(ImageContent {
                data: block["data"].as_str().unwrap_or_default().to_owned(),
                mime_type: block["mimeType"].as_str().unwrap_or("image/png").to_owned(),
            })),
            Some("resource") => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or_default();
                let text = match resource["text"].as_str() {
                    Some(text) => format!("Resource {uri}:\n{text}"),
                    None => format!("Resource {uri} (binary content omitted)"),
                };
                content.push(UserContent::text(text));
            }
            Some("resource_link") => content.push(UserContent::text(format!(
                "{}: {}",
                block["name"].as_str().unwrap_or("resource"),
                block["uri"].as_str().unwrap_or_default()
            ))),
            Some(other) => content.push(UserContent::text(format!("[{other} content omitted]"))),
            None => {}
        }
    }
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        content.push(UserContent::text(structured.to_string()));
    }
    if content.is_empty() {
        content.push(UserContent::text("(no output)"));
    }
    AgentToolResult {
        content,
        is_error: result["isError"].as_bool().unwrap_or(false),
        ..AgentToolResult::default()
    }
}

/// The MCP servers of one project.
pub struct McpHub {
    entries: Mutex<Vec<Entry>>,
    /// Configuration problems.
    errors: Vec<String>,
    changed: watch::Sender<u64>,
}

impl McpHub {
    pub fn new(servers: Vec<Server>, errors: Vec<String>) -> Arc<Self> {
        let entries = servers
            .into_iter()
            .map(|server| Entry {
                state: if server.enabled {
                    ServerState::Connecting
                } else {
                    ServerState::Disabled
                },
                server,
                client: None,
                tools: Vec::new(),
            })
            .collect();
        Arc::new(Self {
            entries: Mutex::new(entries),
            errors,
            changed: watch::channel(0).0,
        })
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.changed.send_modify(|version| *version += 1);
    }

    /// Notified whenever servers connect, fail or change their tools.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Connects every enabled server (call on a Tokio runtime).
    pub fn start(self: &Arc<Self>) {
        let names: Vec<String> = self
            .entries()
            .iter()
            .filter(|e| e.server.enabled)
            .map(|e| e.server.name.clone())
            .collect();
        for name in names {
            let hub = self.clone();
            tokio::spawn(async move { hub.connect(&name).await });
        }
    }

    /// (Re)connects server `name` and lists its tools.
    pub async fn connect(self: &Arc<Self>, name: &str) {
        let server = {
            let mut entries = self.entries();
            let Some(entry) = entries.iter_mut().find(|e| e.server.name == name) else {
                return;
            };
            if let Some(client) = entry.client.take() {
                client.shutdown();
            }
            entry.state = ServerState::Connecting;
            entry.server.clone()
        };
        self.bump();
        let result = async {
            let client = Arc::new(Client::connect(&server).await?);
            let tools = client.list_tools().await?;
            anyhow::Ok((client, tools))
        }
        .await;
        {
            let mut entries = self.entries();
            if let Some(entry) = entries.iter_mut().find(|e| e.server.name == name) {
                match result {
                    Ok((client, tools)) => {
                        entry.client = Some(client);
                        entry.tools = tools;
                        entry.state = ServerState::Connected;
                    }
                    Err(error) => {
                        tracing::warn!(server = name, error = %format!("{error:#}"), "MCP server failed");
                        entry.state = ServerState::Failed(format!("{error:#}"));
                        entry.tools.clear();
                    }
                }
            }
        }
        self.bump();
    }

    async fn refresh_tools(self: &Arc<Self>, name: &str) {
        let client = self
            .entries()
            .iter()
            .find(|e| e.server.name == name)
            .and_then(|e| e.client.clone());
        let Some(client) = client else { return };
        if let Ok(tools) = client.list_tools().await {
            if let Some(entry) = self.entries().iter_mut().find(|e| e.server.name == name) {
                entry.tools = tools;
            }
            self.bump();
        }
    }

    /// The tools of the connected servers.
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn AgentTool>> {
        let entries = self.entries();
        let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
        for entry in entries.iter() {
            let Some(client) = &entry.client else {
                continue;
            };
            for info in &entry.tools {
                tools.push(Arc::new(McpTool {
                    name: tool_name(&entry.server.name, &info.name),
                    server: entry.server.name.clone(),
                    tool: info.name.clone(),
                    description: info.description.clone().unwrap_or_else(|| {
                        format!("{} tool of the {} MCP server", info.name, entry.server.name)
                    }),
                    snippet: format!(
                        "{} (MCP server {})",
                        info.description
                            .as_deref()
                            .and_then(|d| d.lines().next())
                            .unwrap_or(&info.name),
                        entry.server.name
                    ),
                    schema: info.input_schema.clone(),
                    client: client.clone(),
                    hub: Arc::downgrade(self),
                }));
            }
        }
        tools
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.entries()
            .iter()
            .map(|e| ServerStatus {
                name: e.server.name.clone(),
                state: e.state.clone(),
                tools: e.tools.iter().map(|t| t.name.clone()).collect(),
                description: e.server.description.clone(),
                source: e.server.source.clone(),
            })
            .collect()
    }

    /// Stops every server.
    pub fn shutdown(&self) {
        for entry in self.entries().iter_mut() {
            if let Some(client) = entry.client.take() {
                client.shutdown();
            }
        }
    }
}

impl Drop for McpHub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A server tool as an agent tool.
struct McpTool {
    name: String,
    server: String,
    tool: String,
    description: String,
    snippet: String,
    schema: Value,
    client: Arc<Client>,
    hub: Weak<McpHub>,
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        &self.tool
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        let mut schema = self.schema.clone();
        if schema.get("type").is_none() {
            schema["type"] = Value::from("object");
        }
        schema
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(&self.snippet)
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            if self.client.is_closed() {
                if let Some(hub) = self.hub.upgrade() {
                    let server = self.server.clone();
                    tokio::spawn(async move { hub.connect(&server).await });
                }
                anyhow::bail!(
                    "the MCP server `{}` disconnected; reconnecting — try again shortly",
                    self.server
                );
            }
            let result = self.client.call_tool(&self.tool, args, &ctx.cancel).await?;
            if self
                .client
                .tools_changed
                .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(hub) = self.hub.upgrade()
            {
                let server = self.server.clone();
                tokio::spawn(async move { hub.refresh_tools(&server).await });
            }
            Ok(convert_result(&result))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_and_results() {
        assert_eq!(
            tool_name("git-hub", "search.issues"),
            "mcp__git_hub__search_issues"
        );
        let long = tool_name("server", &"x".repeat(80));
        assert_eq!(long.len(), MAX_TOOL_NAME);
        assert_ne!(long, tool_name("server", &"y".repeat(80)));

        let result = convert_result(&json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "AAAA", "mimeType": "image/jpeg"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
                {"type": "audio", "data": "x"}
            ],
            "isError": true
        }));
        assert!(result.is_error);
        assert_eq!(result.content.len(), 4);
        assert_eq!(
            result.content[2].as_text(),
            Some("Resource file:///a:\nbody")
        );
        assert!(matches!(&result.content[1], UserContent::Image(i) if i.mime_type == "image/jpeg"));
        let structured = convert_result(&json!({"content": [], "structuredContent": {"n": 1}}));
        assert_eq!(structured.content[0].as_text(), Some("{\"n\":1}"));
    }
}
