//! Shared HTTP plumbing: client, retries, error formatting and SSE driving.

use std::sync::OnceLock;
use std::time::Duration;

use futures::StreamExt as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::sse::{SseDecoder, SseEvent};

/// Why a request or stream did not complete.
#[derive(Debug)]
pub(crate) enum HttpError {
    /// Cancelled through the request's token.
    Cancelled,
    /// Any other failure, already formatted for display.
    Failed(String),
}

impl From<String> for HttpError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// How long a response may go without sending anything: before its headers arrive, and
/// between chunks of a stream. TCP keepalive already catches dead connections; this
/// catches a provider or proxy that keeps the connection open but stops answering. Long
/// enough for a local server loading a model, or a model thinking without streaming.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest wait before a retry, whatever the server or the backoff ask for.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(IDLE_TIMEOUT)
            .user_agent(concat!("zp/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

/// Builds a header map, skipping entries that are not valid HTTP headers.
pub(crate) fn header_map<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                headers.insert(name, value);
            }
            _ => tracing::warn!(header = name, "skipping invalid request header"),
        }
    }
    headers
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn retry_delay(headers: &HeaderMap, attempt: u32) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        // Capped before the conversion: `Retry-After: 1e20` must not panic.
        .map(|seconds| Duration::from_secs_f64(seconds.min(MAX_RETRY_DELAY.as_secs_f64())))
        .unwrap_or_else(|| backoff(attempt))
}

/// 1 s, 2 s, 4 s, … up to [`MAX_RETRY_DELAY`], for any `attempt`.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1)
        .saturating_mul(2u32.saturating_pow(attempt))
        .min(MAX_RETRY_DELAY)
}

/// A failed request for display; a read timeout says what happened.
fn request_failed(error: &reqwest::Error) -> String {
    if error.is_timeout() && !error.is_connect() {
        format!(
            "no response from the server for {} minutes ({error})",
            IDLE_TIMEOUT.as_secs() / 60
        )
    } else {
        format!("request failed: {error}")
    }
}

/// Longest raw response excerpt included in an error message.
const RAW_EXCERPT_LIMIT: usize = 2000;

/// Extracts a readable message from an error response body.
pub(crate) fn describe_error_body(status: StatusCode, body: &str) -> String {
    let detail = match serde_json::from_str::<Value>(body) {
        Ok(value) => describe_error_json(&value),
        Err(_) => excerpt(body.trim()),
    };
    if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    }
}

/// Describes a JSON error payload: `{"error": {...}}`, `{"error": "..."}` or the error object
/// itself. Proxies often put only a generic code such as `openai_error` in `message`, or wrap
/// the upstream error in `metadata.raw` (OpenRouter); then the details are appended so the
/// real cause is visible.
pub(crate) fn describe_error_json(value: &Value) -> String {
    let error = match value.get("error") {
        Some(error) if error.is_object() || error.is_string() => error,
        _ => value,
    };
    let text = |key: &str| {
        error
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let message = error.as_str().map(str::trim).or_else(|| {
        ["message", "msg", "detail", "error_description", "error_msg"]
            .into_iter()
            .find_map(text)
    });
    let code = text("code").map(str::to_owned).or_else(|| {
        error
            .get("code")
            .filter(|code| code.is_number())
            .map(Value::to_string)
    });
    let kind = text("type").filter(|kind| *kind != "error");

    let mut parts = Vec::new();
    if let Some(message) = message {
        parts.push(message.to_owned());
    }
    let upstream = error.pointer("/metadata/raw").and_then(|raw| match raw {
        Value::String(raw) => Some(raw.trim().to_owned()).filter(|raw| !raw.is_empty()),
        Value::Null => None,
        raw => Some(raw.to_string()),
    });
    if let Some(upstream) = &upstream {
        let provider = error
            .pointer("/metadata/provider_name")
            .and_then(Value::as_str);
        parts.push(match provider {
            Some(provider) => format!("upstream {provider}: {}", excerpt(upstream)),
            None => format!("upstream: {}", excerpt(upstream)),
        });
    }
    let informative = message.is_some_and(|message| {
        !is_generic_error(message) && Some(message) != kind && Some(message) != code.as_deref()
    });
    if !informative && upstream.is_none() {
        // Nothing readable: show the payload itself (it carries type, code, param, ...).
        parts.push(excerpt(&value.to_string()));
    }
    parts.join(" — ")
}

/// Messages that name an error class instead of explaining it.
fn is_generic_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let identifier = !message.contains(char::is_whitespace)
        && (message.contains('_') || message.contains('.'))
        && message
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    identifier
        || matches!(
            lower.trim_end_matches('.'),
            "error"
                | "unknown error"
                | "internal error"
                | "internal server error"
                | "upstream error"
                | "provider returned error"
                | "bad request"
                | "bad response status code"
        )
}

/// At most [`RAW_EXCERPT_LIMIT`] characters of `text`.
fn excerpt(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(RAW_EXCERPT_LIMIT).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// POSTs a JSON body, retrying rate limits and transient failures before the response
/// starts. Returns the successful response.
pub(crate) async fn post_json(
    url: &str,
    headers: HeaderMap,
    body: &Value,
    max_retries: u32,
    cancel: &CancellationToken,
) -> Result<Response, HttpError> {
    let body = serde_json::to_vec(body).map_err(|error| error.to_string())?;
    send(
        || {
            client()
                .post(url)
                .headers(headers.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
        },
        max_retries,
        cancel,
    )
    .await
}

/// Total time allowed for a GET, including the body.
const GET_TIMEOUT: Duration = Duration::from_secs(120);

/// GETs `url` with the same retry policy as [`post_json`]. A `304 Not Modified` is
/// returned like a success so callers can honor conditional requests.
pub(crate) async fn get(
    url: &str,
    headers: HeaderMap,
    max_retries: u32,
    cancel: &CancellationToken,
) -> Result<Response, HttpError> {
    send(
        || {
            client()
                .get(url)
                .headers(headers.clone())
                .timeout(GET_TIMEOUT)
        },
        max_retries,
        cancel,
    )
    .await
}

/// Reads a whole response body as JSON.
pub(crate) async fn read_json(
    response: Response,
    cancel: &CancellationToken,
) -> Result<Value, HttpError> {
    let bytes = tokio::select! {
        () = cancel.cancelled() => return Err(HttpError::Cancelled),
        bytes = response.bytes() => bytes.map_err(|error| format!("reading response failed: {error}"))?,
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| HttpError::Failed(format!("invalid JSON response: {error}")))
}

async fn send(
    request: impl Fn() -> reqwest::RequestBuilder,
    max_retries: u32,
    cancel: &CancellationToken,
) -> Result<Response, HttpError> {
    let mut attempt = 0;
    loop {
        let result = tokio::select! {
            () = cancel.cancelled() => return Err(HttpError::Cancelled),
            result = request().send() => result,
        };
        let delay = match result {
            Ok(response)
                if response.status().is_success()
                    || response.status() == StatusCode::NOT_MODIFIED =>
            {
                return Ok(response);
            }
            Ok(response) => {
                let status = response.status();
                if attempt >= max_retries || !is_retryable(status) {
                    let text = tokio::select! {
                        () = cancel.cancelled() => return Err(HttpError::Cancelled),
                        text = response.text() => text.unwrap_or_default(),
                    };
                    return Err(describe_error_body(status, &text).into());
                }
                tracing::debug!(%status, attempt, "retrying request");
                retry_delay(response.headers(), attempt)
            }
            Err(error) => {
                // Only failures to connect are retried: the request was not sent. After a
                // read timeout the server may be generating (and billing) already.
                if attempt >= max_retries || !error.is_connect() {
                    return Err(request_failed(&error).into());
                }
                tracing::debug!(%error, attempt, "retrying request");
                backoff(attempt)
            }
        };
        attempt += 1;
        tokio::select! {
            () = cancel.cancelled() => return Err(HttpError::Cancelled),
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Reads an SSE response body, calling `on_event` for every event until the body ends.
pub(crate) async fn drive_sse(
    response: Response,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(SseEvent) -> Result<(), String>,
) -> Result<(), HttpError> {
    let is_json = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    if is_json {
        // Some proxies answer a streaming request with a plain JSON error and status 200.
        let text = tokio::select! {
            () = cancel.cancelled() => return Err(HttpError::Cancelled),
            text = response.text() => text.map_err(|error| format!("reading response failed: {error}"))?,
        };
        return Err(match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.get("error").is_some_and(|e| !e.is_null()) => {
                describe_error_json(&value)
            }
            _ => format!(
                "expected an event stream, got JSON: {}",
                excerpt(text.trim())
            ),
        }
        .into());
    }
    let mut decoder = SseDecoder::new();
    let mut body = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            () = cancel.cancelled() => return Err(HttpError::Cancelled),
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => {
                for event in decoder.push(&bytes) {
                    on_event(event)?;
                }
            }
            Some(Err(error)) if error.is_timeout() => {
                return Err(format!(
                    "the stream stalled: nothing received for {} minutes ({error})",
                    IDLE_TIMEOUT.as_secs() / 60
                )
                .into());
            }
            Some(Err(error)) => return Err(format!("stream interrupted: {error}").into()),
            None => break,
        }
    }
    if let Some(event) = decoder.finish() {
        on_event(event)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[test]
    fn retry_delays_are_capped_whatever_the_server_says() {
        let response = |retry_after: &str| header_map([("retry-after", retry_after)]);
        assert_eq!(retry_delay(&response("1e20"), 0), MAX_RETRY_DELAY);
        assert_eq!(
            retry_delay(&response("2.5"), 0),
            Duration::from_millis(2500)
        );
        assert_eq!(retry_delay(&response("soon"), 1), Duration::from_secs(2));
        assert_eq!(
            (0..3).map(backoff).collect::<Vec<_>>(),
            [1, 2, 4].map(Duration::from_secs)
        );
        assert_eq!(backoff(64), MAX_RETRY_DELAY);
        assert_eq!(backoff(u32::MAX), MAX_RETRY_DELAY);
    }

    /// A server that accepts connections, reads the request and, after sending `head`
    /// (maybe nothing), goes quiet. Returns its URL and how many connections it accepted.
    async fn stalling_server(head: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let _ = socket.read(&mut buffer).await;
                    let _ = socket.write_all(head.as_bytes()).await;
                    tokio::time::sleep(Duration::from_secs(60)).await;
                });
            }
        });
        (url, accepted)
    }

    fn quick_client() -> reqwest::Client {
        reqwest::Client::builder()
            .read_timeout(Duration::from_millis(200))
            .build()
            .unwrap()
    }

    /// A server that never answers fails the request after the idle timeout, and the POST
    /// is not sent again: the server may already be generating.
    #[tokio::test]
    async fn a_silent_server_times_out_without_a_retry() {
        let (url, accepted) = stalling_server("").await;
        let client = quick_client();
        let result = send(
            || client.post(&url).body("{}"),
            2,
            &CancellationToken::new(),
        )
        .await;
        let Err(HttpError::Failed(message)) = result else {
            panic!("expected a failure");
        };
        assert!(message.contains("no response from the server"), "{message}");
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "retried a sent request");
    }

    /// A stream that stops sending fails instead of hanging.
    #[tokio::test]
    async fn a_stalled_stream_fails() {
        let (url, _) = stalling_server(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"a\":1}\n\n",
        )
        .await;
        let response = quick_client().post(&url).send().await.unwrap();
        let mut events = 0;
        let result = drive_sse(response, &CancellationToken::new(), |_| {
            events += 1;
            Ok(())
        })
        .await;
        let Err(HttpError::Failed(message)) = result else {
            panic!("expected a failure");
        };
        assert_eq!(events, 1);
        assert!(message.contains("the stream stalled"), "{message}");
    }

    /// Failing to connect means nothing was sent, so it is retried.
    #[tokio::test]
    async fn connection_failures_are_retried() {
        // Reserve the port without listening so another test cannot claim it.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let url = format!("http://{}/v1", socket.local_addr().unwrap());
        let attempts = AtomicUsize::new(0);
        // A read timeout can fire while Windows is still trying to connect.
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let result = send(
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                client.post(&url).body("{}")
            },
            1,
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(&result, Err(HttpError::Failed(message)) if message.starts_with("request failed")),
            "expected a connection failure, got {result:?}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() >= Duration::from_secs(1), "backed off");
    }

    #[test]
    fn the_shared_client_has_an_idle_timeout() {
        assert!(
            format!("{:?}", client()).contains(&format!("read_timeout: {IDLE_TIMEOUT:?}")),
            "{:?}",
            client()
        );
    }

    #[test]
    fn error_bodies_are_summarized() {
        let status = StatusCode::UNAUTHORIZED;
        assert_eq!(
            describe_error_body(
                status,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            ),
            "HTTP 401 Unauthorized: invalid x-api-key"
        );
        assert_eq!(
            describe_error_body(status, r#"{"error":"bad key"}"#),
            "HTTP 401 Unauthorized: bad key"
        );
        assert_eq!(
            describe_error_body(status, "nope\n"),
            "HTTP 401 Unauthorized: nope"
        );
        assert_eq!(describe_error_body(status, ""), "HTTP 401 Unauthorized");
    }

    #[test]
    fn generic_errors_show_the_payload() {
        let body = r#"{"error":{"message":"openai_error","type":"bad_response_status_code","code":"bad_response_status_code"}}"#;
        let text = describe_error_body(StatusCode::BAD_REQUEST, body);
        assert!(
            text.starts_with("HTTP 400 Bad Request: openai_error — {"),
            "{text}"
        );
        assert!(text.contains("bad_response_status_code"), "{text}");

        let body = r#"{"error":{"type":"openai_error","code":500}}"#;
        let text = describe_error_body(StatusCode::INTERNAL_SERVER_ERROR, body);
        assert!(text.contains(r#""type":"openai_error""#), "{text}");

        let body = r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"error\":\"context too long\"}","provider_name":"Acme"}}}"#;
        assert_eq!(
            describe_error_body(StatusCode::BAD_REQUEST, body),
            r#"HTTP 400 Bad Request: Provider returned error — upstream Acme: {"error":"context too long"}"#
        );

        let value: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )
        .unwrap();
        assert_eq!(describe_error_json(&value), "Overloaded");
    }
}
