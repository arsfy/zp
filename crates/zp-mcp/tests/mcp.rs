//! MCP over stdio (a shell script server) and streamable HTTP (an in-process server).

#[cfg(unix)]
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;
use zp_agent::ToolContext;
use zp_mcp::{McpHub, Server, ServerState, Transport};

/// A stdio MCP server in POSIX shell: answers by matching the method in each line.
#[cfg(unix)]
const SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      echo "mock starting" >&2
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"1"},"instructions":"Mock tools"}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","method":"ping","id":"srv-1"}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},{"name":"fail","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"name":"echo"'*)
      text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: %s"}]}}\n' "$id" "$text" ;;
    *'"name":"fail"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"it broke"}],"isError":true}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"unknown tool"}}\n' "$id" ;;
  esac
done
"#;

fn ctx() -> ToolContext {
    ToolContext::detached("call-1", CancellationToken::new())
}

fn server(name: &str, transport: Transport) -> Server {
    Server {
        name: name.into(),
        transport,
        enabled: true,
        timeout_secs: 5,
        description: None,
        source: PathBuf::from("mcp.json"),
    }
}

async fn wait_connected(hub: &std::sync::Arc<McpHub>) {
    let mut changes = hub.subscribe();
    for _ in 0..50 {
        if hub
            .statuses()
            .iter()
            .all(|s| !matches!(s.state, ServerState::Connecting))
        {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(200), changes.changed()).await;
    }
    panic!("servers did not connect: {:?}", hub.statuses());
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_server_tools() {
    let stdio = Transport::Stdio {
        command: "sh".into(),
        args: vec!["-c".into(), SCRIPT.into()],
        env: BTreeMap::new(),
        cwd: None,
    };
    let broken = Transport::Stdio {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "echo 'cannot start: missing token' >&2; exit 3".into(),
        ],
        env: BTreeMap::new(),
        cwd: None,
    };
    // Stops reading before it explains itself, so the request fails (broken pipe) before
    // its stderr arrives.
    let closes_input = Transport::Stdio {
        command: "sh".into(),
        args: vec![
            "-c".into(),
            "exec <&-; sleep 0.2; echo 'bad config file' >&2; exit 1".into(),
        ],
        env: BTreeMap::new(),
        cwd: None,
    };
    let mut disabled = server("off", stdio.clone());
    disabled.enabled = false;
    let hub = McpHub::new(
        vec![
            server("mock", stdio),
            server("broken", broken),
            disabled,
            server("closes-input", closes_input),
        ],
        Vec::new(),
    );
    hub.start();
    wait_connected(&hub).await;

    let statuses = hub.statuses();
    assert_eq!(statuses[0].state, ServerState::Connected);
    assert_eq!(statuses[0].tools, ["echo", "fail"]);
    let ServerState::Failed(error) = &statuses[1].state else {
        panic!("{:?}", statuses[1])
    };
    assert!(error.contains("cannot start: missing token"), "{error}");
    assert_eq!(statuses[2].state, ServerState::Disabled);
    let ServerState::Failed(error) = &statuses[3].state else {
        panic!("{:?}", statuses[3])
    };
    assert!(error.contains("bad config file"), "{error}");

    let tools = hub.tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["mcp__mock__echo", "mcp__mock__fail"]);
    assert_eq!(tools[0].description(), "Echo text back");
    assert_eq!(tools[0].parameters()["required"], json!(["text"]));
    assert_eq!(tools[1].parameters()["type"], "object");

    let result = tools[0]
        .execute(json!({"text": "hi"}), ctx())
        .await
        .unwrap();
    assert_eq!(result.content[0].as_text(), Some("echo: hi"));
    assert!(!result.is_error);
    let failed = tools[1].execute(json!({}), ctx()).await.unwrap();
    assert!(failed.is_error);

    hub.shutdown();
    let error = tools[0]
        .execute(json!({"text": "x"}), ctx())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("disconnected"), "{error}");
}

/// Serves `responses` (one per request, in order) as an MCP HTTP endpoint; returns the
/// URL and the received request bodies.
async fn http_server(
    responses: Vec<(&'static str, String)>,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<(Value, Option<String>)>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut responses = responses.into_iter();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read headers and body (Content-Length).
            let (head, body) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let head = text[..end].to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    if buffer.len() >= end + 4 + length {
                        break (head, text[end + 4..end + 4 + length].to_owned());
                    }
                }
            };
            let session = head
                .lines()
                .find_map(|l| l.strip_prefix("mcp-session-id:"))
                .map(|v| v.trim().to_owned());
            let request: Value = serde_json::from_str(&body).unwrap();
            let _ = sender.send((request.clone(), session));
            let reply = if request.get("id").is_none() {
                "HTTP/1.1 202 Accepted\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_owned()
            } else {
                let (kind, payload) = responses.next().unwrap();
                let payload = payload.replace("$ID", &request["id"].to_string());
                let body = if kind.starts_with("sse") {
                    let events = format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{}}}}\n\nevent: message\ndata: {payload}\n\n"
                    );
                    // Python servers (sse-starlette) end lines with CRLF.
                    if kind == "sse-crlf" {
                        events.replace('\n', "\r\n")
                    } else {
                        events
                    }
                } else {
                    payload
                };
                let content_type = if kind.starts_with("sse") {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                format!(
                    "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: {content_type}\r\nmcp-session-id: s-42\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
            };
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (url, receiver)
}

#[tokio::test]
async fn http_server_tools() {
    let (url, mut requests) = http_server(vec![
        ("json", r#"{"jsonrpc":"2.0","id":$ID,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.into()),
        ("sse", r#"{"jsonrpc":"2.0","id":$ID,"result":{"tools":[{"name":"search","description":"Search docs","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}]}}"#.into()),
        ("sse-crlf", r#"{"jsonrpc":"2.0","id":$ID,"result":{"content":[{"type":"text","text":"3 results 中文"}]}}"#.into()),
    ])
    .await;
    let hub = McpHub::new(
        vec![server(
            "docs",
            Transport::Http {
                url,
                headers: [("Authorization".to_owned(), "Bearer t".to_owned())].into(),
            },
        )],
        Vec::new(),
    );
    hub.start();
    wait_connected(&hub).await;
    assert_eq!(hub.statuses()[0].state, ServerState::Connected);
    let tools = hub.tools();
    assert_eq!(tools[0].name(), "mcp__docs__search");
    let result = tools[0].execute(json!({"q": "mcp"}), ctx()).await.unwrap();
    assert_eq!(result.content[0].as_text(), Some("3 results 中文"));

    let mut methods = Vec::new();
    while let Ok((request, session)) = requests.try_recv() {
        methods.push((request["method"].as_str().unwrap().to_owned(), session));
    }
    assert_eq!(
        methods,
        [
            ("initialize".into(), None),
            ("notifications/initialized".into(), Some("s-42".into())),
            ("tools/list".into(), Some("s-42".into())),
            ("tools/call".into(), Some("s-42".into())),
        ]
    );
}
