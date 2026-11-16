//! Signing in to providers with OAuth instead of an API key (ported from pi's login
//! flows).
//!
//! An [`OAuthFlow`] signs in through the browser (authorization code with PKCE and a
//! loopback callback, or a pasted redirect URL when the browser cannot reach it) or with
//! a device code, refreshes the resulting tokens and turns them into [`RequestAuth`].
//! [`crate::Models`] refreshes expired tokens before requests and stores them back.
//!
//! Callers drive a login with a [`LoginInteraction`]: it reports [`LoginEvent`]s (a URL
//! to open, a code to enter) and accepts a pasted code or redirect URL.

mod anthropic;
mod callback;
mod device;
mod github_copilot;
mod kimi;
mod openai_chatgpt;
mod openai_codex;
mod openrouter;
mod xai;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures::future::BoxFuture;
use serde_json::Value;
use sha2::Digest as _;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::auth::{OAuthCredential, RequestAuth};
use crate::model::Model;

pub(crate) use github_copilot::dynamic_headers as copilot_dynamic_headers;
pub(crate) use openai_codex::{ORIGINATOR as CODEX_ORIGINATOR, account_id as codex_account_id};

pub use anthropic::AnthropicOAuth;
pub use github_copilot::GitHubCopilotOAuth;
pub use kimi::KimiCodingOAuth;
pub use openai_chatgpt::OpenAiChatGptOAuth;
pub use openai_codex::OpenAiCodexOAuth;
pub use openrouter::OpenRouterOAuth;
pub use xai::XaiOAuth;

/// The message of a cancelled login.
pub const LOGIN_CANCELLED: &str = "Login cancelled";

/// Signing in to one provider.
pub trait OAuthFlow: Send + Sync {
    /// What signing in gives access to, e.g. `Claude Pro/Max`.
    fn name(&self) -> &str;

    /// The sign-in action, e.g. `Sign in with Claude`.
    fn login_label(&self) -> &str;

    /// Runs the login until tokens are issued (or it fails or is cancelled).
    fn login(
        &self,
        interaction: LoginInteraction,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>>;

    /// Exchanges the refresh token for new tokens.
    fn refresh(
        &self,
        credential: OAuthCredential,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>>;

    /// How requests authenticate with a valid credential.
    fn request_auth(&self, credential: &OAuthCredential) -> RequestAuth {
        RequestAuth::api_key(credential.access.clone())
    }

    /// The models the signed-in account can use.
    fn filter_models(&self, credential: &OAuthCredential, models: Vec<Model>) -> Vec<Model> {
        let _ = credential;
        models
    }
}

/// Progress of a login, for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginEvent {
    /// Open `url` in the browser. Pasting the final redirect URL (or the code the page
    /// shows) is accepted too.
    AuthUrl { url: String, instructions: String },
    /// Enter `user_code` at `verification_uri`.
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    /// A step is running.
    Progress(String),
}

/// What a login may need from the app.
#[derive(Clone, Debug, Default)]
pub struct LoginOptions {
    /// A stable UUID of this installation (Sign in with ChatGPT identifies the app with it).
    pub device_id: Option<String>,
    /// The name the app introduces itself with (defaults to `ZP`).
    pub agent_name: Option<String>,
}

/// The app side of a login: events out, a pasted code in, cancellation.
#[derive(Clone)]
pub struct LoginInteraction {
    cancel: CancellationToken,
    notify: Arc<dyn Fn(LoginEvent) + Send + Sync>,
    manual: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
    options: LoginOptions,
}

/// Submits what the user pasted to a running login.
#[derive(Clone)]
pub struct ManualInput(mpsc::UnboundedSender<String>);

impl ManualInput {
    /// Hands `text` (a code or redirect URL) to the login. Returns `false` once the login
    /// is over.
    pub fn submit(&self, text: impl Into<String>) -> bool {
        self.0.send(text.into()).is_ok()
    }
}

impl LoginInteraction {
    /// An interaction reporting events to `notify`. Keep the returned [`ManualInput`] to
    /// let the user paste a code; drop it if they cannot.
    pub fn new(
        cancel: CancellationToken,
        notify: impl Fn(LoginEvent) + Send + Sync + 'static,
        options: LoginOptions,
    ) -> (Self, ManualInput) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                cancel,
                notify: Arc::new(notify),
                manual: Arc::new(Mutex::new(receiver)),
                options,
            },
            ManualInput(sender),
        )
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn options(&self) -> &LoginOptions {
        &self.options
    }

    pub(crate) fn notify(&self, event: LoginEvent) {
        (self.notify)(event);
    }

    pub(crate) fn progress(&self, message: impl Into<String>) {
        self.notify(LoginEvent::Progress(message.into()));
    }

    /// The next non-empty pasted text; never resolves when nothing can be pasted.
    pub(crate) async fn manual_input(&self) -> String {
        let mut receiver = self.manual.lock().await;
        loop {
            match receiver.recv().await {
                Some(text) if !text.trim().is_empty() => return text.trim().to_owned(),
                Some(_) => {}
                None => return std::future::pending().await,
            }
        }
    }

    /// Fails with [`LOGIN_CANCELLED`] once cancelled.
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            Err(LOGIN_CANCELLED.into())
        } else {
            Ok(())
        }
    }
}

/// The built-in sign-in flow of `provider`, if it has one.
pub fn builtin_flow(provider: &str) -> Option<Arc<dyn OAuthFlow>> {
    Some(match provider {
        "anthropic" => Arc::new(AnthropicOAuth),
        "openai" => Arc::new(OpenAiChatGptOAuth),
        "openai-codex" => Arc::new(OpenAiCodexOAuth),
        "github-copilot" => Arc::new(GitHubCopilotOAuth),
        "kimi-coding" => Arc::new(KimiCodingOAuth),
        "xai" => Arc::new(XaiOAuth),
        "openrouter" => Arc::new(OpenRouterOAuth),
        _ => return None,
    })
}

// ----- helpers shared by the flows -------------------------------------------------------

/// Random bytes (from v4 UUIDs, which come from the OS generator).
pub(crate) fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len + 16);
    while bytes.len() < len {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    bytes.truncate(len);
    bytes
}

pub(crate) fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A PKCE verifier and its S256 challenge.
pub(crate) fn pkce() -> (String, String) {
    let verifier = base64url(&random_bytes(32));
    let challenge = base64url(&sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// The claims of a JWT (unverified).
pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `url?key=value&…` with the values encoded.
pub(crate) fn url_with_params(url: &str, params: &[(&str, &str)]) -> String {
    reqwest::Url::parse_with_params(url, params)
        .map(String::from)
        .unwrap_or_else(|_| url.to_owned())
}

/// `key=value&…`, form-encoded.
pub(crate) fn form_encode(params: &[(&str, &str)]) -> String {
    url_with_params("http://x/", params)
        .split_once('?')
        .map(|(_, query)| query.to_owned())
        .unwrap_or_default()
}

/// The code (and state) in a pasted redirect URL, `code#state`, query string or bare code.
pub(crate) fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Ok(url) = reqwest::Url::parse(value)
        && url.has_host()
    {
        let get = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        return (get("code"), get("state"));
    }
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_owned()), Some(state.to_owned()));
    }
    if value.contains("code=") {
        let url = format!("http://x/?{value}");
        if let Ok(url) = reqwest::Url::parse(&url) {
            let get = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            return (get("code"), get("state"));
        }
    }
    (Some(value.to_owned()), None)
}

/// A finished HTTP exchange: status and body text.
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) text: String,
}

impl Reply {
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as a JSON object (`{}` when it is not one).
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str::<Value>(&self.text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Default::default()))
    }

    /// `failed (HTTP 400): body` for errors.
    pub(crate) fn failure(&self, what: &str) -> String {
        let body = self.text.trim();
        let body: String = body.chars().take(500).collect();
        if body.is_empty() {
            format!("{what} failed (HTTP {})", self.status)
        } else {
            format!("{what} failed (HTTP {}): {body}", self.status)
        }
    }
}

/// A request body.
pub(crate) enum Body<'a> {
    Form(&'a [(&'a str, &'a str)]),
    Json(&'a Value),
    None,
}

/// Sends a request (30 s timeout) and reads the reply.
pub(crate) async fn send(
    method: reqwest::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Body<'_>,
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    let mut request = crate::http::client()
        .request(method, url)
        .timeout(Duration::from_secs(30))
        .header("accept", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request = match body {
        Body::Form(params) => request
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form_encode(params)),
        Body::Json(value) => request
            .header("content-type", "application/json")
            .body(value.to_string()),
        Body::None => request,
    };
    let exchange = async {
        let response = request
            .send()
            .await
            .map_err(|error| format!("request to {url} failed: {error}"))?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        Ok(Reply { status, text })
    };
    tokio::select! {
        () = cancel.cancelled() => Err(LOGIN_CANCELLED.into()),
        reply = exchange => reply,
    }
}

pub(crate) async fn post_form(
    url: &str,
    params: &[(&str, &str)],
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    send(reqwest::Method::POST, url, &[], Body::Form(params), cancel).await
}

pub(crate) async fn post_json(
    url: &str,
    body: &Value,
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    send(reqwest::Method::POST, url, &[], Body::Json(body), cancel).await
}

/// A string field of a JSON object.
pub(crate) fn str_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// A positive number field (also accepted as a numeric string).
pub(crate) fn number_field(value: &Value, field: &str) -> Option<f64> {
    let value = value.get(field)?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// Waits for the browser callback or for a pasted code, whichever comes first.
pub(crate) async fn callback_or_manual(
    interaction: &LoginInteraction,
    callback: Option<callback::CallbackServer>,
    expected_state: Option<&str>,
    provider: &str,
) -> Result<Authorization, String> {
    let from_browser = async {
        match callback {
            Some(server) => server.wait(expected_state, provider).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = interaction.cancel().cancelled() => Err(LOGIN_CANCELLED.into()),
        result = from_browser => result.map(Authorization::Callback),
        text = interaction.manual_input() => Ok(Authorization::Manual(text)),
    }
}

/// How the authorization code arrived.
pub(crate) enum Authorization {
    /// The browser reached the loopback server; the callback's query parameters.
    Callback(callback::CallbackParams),
    /// The user pasted a code or redirect URL.
    Manual(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_hashed_verifier() {
        let (verifier, challenge) = pkce();
        assert_eq!(verifier.len(), 43);
        let expected = base64url(&sha2::Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        assert_ne!(pkce().0, verifier);
    }

    #[test]
    fn authorization_input_forms() {
        assert_eq!(
            parse_authorization_input("http://localhost:1/cb?code=abc&state=xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_authorization_input("abc#xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_authorization_input("code=abc&state=xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_authorization_input(" abc "),
            (Some("abc".into()), None)
        );
        assert_eq!(parse_authorization_input(""), (None, None));
    }

    #[test]
    fn jwt_claims_are_decoded() {
        let payload = base64url(br#"{"sub":"me","n":1}"#);
        let token = format!("h.{payload}.s");
        assert_eq!(jwt_claims(&token).unwrap()["sub"], "me");
        assert!(jwt_claims("nope").is_none());
    }

    #[test]
    fn form_encoding_escapes_values() {
        assert_eq!(form_encode(&[("a", "x y"), ("b", "c&d")]), "a=x+y&b=c%26d");
    }

    #[test]
    fn every_builtin_flow_is_found() {
        for id in [
            "anthropic",
            "openai",
            "openai-codex",
            "github-copilot",
            "kimi-coding",
            "xai",
            "openrouter",
        ] {
            assert!(builtin_flow(id).is_some(), "{id}");
        }
        assert!(builtin_flow("deepseek").is_none());
    }
}
