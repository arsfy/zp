//! A minimal mock HTTP server shared by the integration tests.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// A canned HTTP response, written in parts with an optional pause between them.
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub parts: Vec<String>,
    pub pause: Duration,
}

impl Reply {
    pub fn sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/event-stream".into())],
            parts: vec![body.to_owned()],
            pause: Duration::ZERO,
        }
    }

    pub fn json(body: &Value) -> Self {
        Self::status(200, &body.to_string())
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json".into())],
            parts: vec![body.to_owned()],
            pause: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub request_line: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Serves `replies` in order (one per connection) and records requests.
pub async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let log = recorded.clone();
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or_default().to_owned();
            let headers: Vec<(String, String)> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
                .collect();
            let length: usize = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            while buffer.len() < header_end + length {
                let n = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..n]);
            }
            let body = serde_json::from_slice(&buffer[header_end..header_end + length])
                .unwrap_or(Value::Null);
            log.lock().unwrap().push(Recorded {
                request_line,
                headers,
                body,
            });

            let mut head = format!("HTTP/1.1 {} Mock\r\nconnection: close\r\n", reply.status);
            for (name, value) in &reply.headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            if socket.write_all(head.as_bytes()).await.is_err() {
                continue;
            }
            for part in &reply.parts {
                if socket.write_all(part.as_bytes()).await.is_err() {
                    break;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(reply.pause).await;
            }
            let _ = socket.shutdown().await;
        }
    });
    (address, recorded)
}
