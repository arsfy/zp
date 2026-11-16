//! The loopback server that receives the browser's OAuth redirect.

use std::collections::HashMap;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// The redirect's query parameters.
pub(crate) type CallbackParams = HashMap<String, String>;

/// Host the callback server listens on (`ZP_OAUTH_CALLBACK_HOST` overrides it, e.g. for
/// containers).
pub(crate) fn callback_host() -> String {
    std::env::var("ZP_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_owned())
}

/// A bound, not yet answered callback endpoint.
pub(crate) struct CallbackServer {
    listener: TcpListener,
    path: String,
}

impl CallbackServer {
    /// Listens on `port` (0 for any free port) for requests to `path`.
    pub(crate) async fn bind(port: u16, path: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind((callback_host().as_str(), port)).await?;
        Ok(Self {
            listener,
            path: path.to_owned(),
        })
    }

    /// The bound port.
    pub(crate) fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }

    /// `http://{host}:{port}{path}`.
    pub(crate) fn redirect_uri(&self, host: &str) -> String {
        format!("http://{host}:{}{}", self.port(), self.path)
    }

    /// Answers requests until one carries an authorization code (with `expected_state`,
    /// when given) or an error.
    pub(crate) async fn wait(
        self,
        expected_state: Option<&str>,
        provider: &str,
    ) -> Result<CallbackParams, String> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|error| format!("the sign-in callback failed: {error}"))?;
            let Some(target) = read_request_target(&mut stream).await else {
                continue;
            };
            let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
                respond(&mut stream, 400, &error_page("Invalid request.", "")).await;
                continue;
            };
            if url.path() != self.path {
                respond(
                    &mut stream,
                    404,
                    &error_page("Callback route not found.", ""),
                )
                .await;
                continue;
            }
            let params: CallbackParams = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if let Some(expected) = expected_state
                && params.get("state").map(String::as_str) != Some(expected)
            {
                respond(&mut stream, 400, &error_page("State mismatch.", "")).await;
                continue;
            }
            if let Some(error) = params.get("error") {
                let detail = params.get("error_description").unwrap_or(error).clone();
                respond(
                    &mut stream,
                    400,
                    &error_page(&format!("{provider} authorization failed."), &detail),
                )
                .await;
                return Err(format!("{provider} authorization failed: {detail}"));
            }
            if !params.contains_key("code") {
                respond(
                    &mut stream,
                    400,
                    &error_page("Missing authorization code.", ""),
                )
                .await;
                continue;
            }
            respond(&mut stream, 200, &success_page(provider)).await;
            return Ok(params);
        }
    }
}

/// Reads a request head and returns its target (`/path?query`) if it is a GET.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = async {
        while !buffer.windows(4).any(|w| w == b"\r\n\r\n") && buffer.len() < 16 * 1024 {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
        }
        Some(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), read)
        .await
        .ok()??;
    let head = String::from_utf8_lossy(&buffer);
    let mut parts = head.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then(|| parts.next().map(str::to_owned))?
}

async fn respond(stream: &mut TcpStream, status: u16, html: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>ZP</title>\
         <style>body{{font:15px -apple-system,system-ui,sans-serif;display:flex;\
         align-items:center;justify-content:center;height:90vh;color:#222}}\
         p{{color:#666}}</style></head><body><div><h2>{}</h2><p>{}</p></div></body></html>",
        escape(title),
        escape(detail)
    )
}

fn success_page(provider: &str) -> String {
    page(
        &format!("Signed in to {provider}"),
        "You can close this page and return to ZP.",
    )
}

fn error_page(title: &str, detail: &str) -> String {
    page(title, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nhost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn waits_for_a_matching_callback() {
        let server = CallbackServer::bind(0, "/callback").await.unwrap();
        let port = server.port();
        assert_eq!(
            server.redirect_uri("localhost"),
            format!("http://localhost:{port}/callback")
        );
        let wait = tokio::spawn(server.wait(Some("s1"), "Test"));
        assert!(get(port, "/other").await.starts_with("HTTP/1.1 404"));
        assert!(
            get(port, "/callback?code=x&state=bad")
                .await
                .starts_with("HTTP/1.1 400")
        );
        assert!(
            get(port, "/callback?state=s1")
                .await
                .starts_with("HTTP/1.1 400")
        );
        let ok = get(port, "/callback?code=abc&state=s1").await;
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("Signed in to Test"));
        let params = wait.await.unwrap().unwrap();
        assert_eq!(params["code"], "abc");
    }

    #[tokio::test]
    async fn an_error_ends_the_wait() {
        let server = CallbackServer::bind(0, "/cb").await.unwrap();
        let port = server.port();
        let wait = tokio::spawn(server.wait(None, "Test"));
        get(port, "/cb?error=access_denied&error_description=nope").await;
        assert_eq!(
            wait.await.unwrap().unwrap_err(),
            "Test authorization failed: nope"
        );
    }
}
