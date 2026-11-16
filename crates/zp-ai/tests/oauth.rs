//! Signed-in providers: token refresh before requests, and the request shapes that
//! sign-ins need (Claude Code identity, Codex backend, Responses API).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use common::{Reply, serve};
use futures::future::BoxFuture;
use serde_json::json;
use zp_ai::oauth::{LoginInteraction, OAuthFlow};
use zp_ai::{
    AuthSource, CancellationToken, Context, Credential, CredentialStore, HttpProvider,
    MemoryCredentialStore, Message, Model, Models, OAuthCredential, StopReason, StreamOptions,
    Tool,
};

/// A flow whose refresh issues `new-…` tokens and counts calls.
struct FakeFlow {
    prefix: &'static str,
    refreshes: AtomicUsize,
}

impl OAuthFlow for FakeFlow {
    fn name(&self) -> &str {
        "Fake"
    }

    fn login_label(&self) -> &str {
        "Sign in"
    }

    fn login(
        &self,
        _interaction: LoginInteraction,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
        let access = format!("{}login", self.prefix);
        Box::pin(async move { Ok(OAuthCredential::new(access, "refresh", 3600.0, 0)) })
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        let access = format!("{}new", self.prefix);
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(OAuthCredential::new(access, credential.refresh, 3600.0, 0))
        })
    }
}

fn model(api: &str, provider: &str, base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": "mock-model",
        "name": "Mock",
        "api": api,
        "provider": provider,
        "baseUrl": base_url,
        "reasoning": true,
        "input": ["text"],
        "contextWindow": 100000,
        "maxTokens": 1000
    }))
    .unwrap()
}

fn context() -> Context {
    Context {
        system_prompt: Some("sys".into()),
        messages: vec![Message::user("hello")],
        tools: vec![Tool {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }],
    }
}

fn expired(access: &str) -> Credential {
    let mut credential = OAuthCredential::new(access, "refresh", 0.0, 0);
    credential.expires -= 1000;
    Credential::OAuth(credential)
}

fn setup(
    model: &Model,
    prefix: &'static str,
) -> (Models, Arc<MemoryCredentialStore>, Arc<FakeFlow>) {
    let store = Arc::new(MemoryCredentialStore::new());
    let flow = Arc::new(FakeFlow {
        prefix,
        refreshes: AtomicUsize::new(0),
    });
    let mut models = Models::new()
        .with_env(Arc::new(|_| None))
        .with_credentials(store.clone());
    models.set_provider(Arc::new(
        HttpProvider::new(
            &model.provider,
            "Mock",
            Vec::<String>::new(),
            vec![model.clone()],
        )
        .with_oauth(flow.clone()),
    ));
    (models, store, flow)
}

const ANTHROPIC_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\",\"input\":{}}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

#[tokio::test]
async fn claude_sign_in_refreshes_once_and_talks_like_claude_code() {
    let (url, recorded) = serve(vec![Reply::sse(ANTHROPIC_SSE), Reply::sse(ANTHROPIC_SSE)]).await;
    let model = model("anthropic-messages", "anthropic", &url);
    let (models, store, flow) = setup(&model, "sk-ant-oat-");
    store.set_credential("anthropic", expired("sk-ant-oat-old"));
    assert_eq!(
        models.resolve_auth("anthropic").map(|a| a.source),
        Some(AuthSource::OAuth)
    );

    // Two requests at once refresh the expired token only once.
    let context = context();
    let (first, second) = tokio::join!(
        models.complete(&model, &context, StreamOptions::default()),
        models.complete(&model, &context, StreamOptions::default()),
    );
    for message in [&first, &second] {
        assert_eq!(
            message.stop_reason,
            StopReason::ToolUse,
            "{:?}",
            message.error_message
        );
        // Claude Code's tool casing is mapped back to the declared name.
        assert_eq!(message.tool_calls().next().unwrap().name, "read");
    }
    assert_eq!(flow.refreshes.load(Ordering::SeqCst), 1);
    let Some(Credential::OAuth(stored)) = store.credential("anthropic") else {
        panic!("still signed in")
    };
    assert_eq!(stored.access, "sk-ant-oat-new");

    let requests = recorded.lock().unwrap();
    let request = &requests[0];
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-ant-oat-new")
    );
    assert_eq!(request.header("x-api-key"), None);
    assert!(
        request
            .header("anthropic-beta")
            .unwrap()
            .contains("oauth-2025-04-20")
    );
    assert!(
        request
            .header("user-agent")
            .unwrap()
            .starts_with("claude-cli/")
    );
    assert_eq!(
        request.body["system"][0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(request.body["system"][1]["text"], "sys");
    assert_eq!(request.body["tools"][0]["name"], "Read");
}

#[tokio::test]
async fn a_failed_refresh_asks_to_sign_in_again() {
    struct Failing;
    impl OAuthFlow for Failing {
        fn name(&self) -> &str {
            "Failing"
        }
        fn login_label(&self) -> &str {
            "Sign in"
        }
        fn login(
            &self,
            _: LoginInteraction,
        ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
            Box::pin(async { Err("no".into()) })
        }
        fn refresh(
            &self,
            _: OAuthCredential,
            _: CancellationToken,
        ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
            Box::pin(async { Err("invalid_grant".into()) })
        }
    }
    let model = model("anthropic-messages", "anthropic", "http://127.0.0.1:9");
    let store = Arc::new(MemoryCredentialStore::new());
    store.set_credential("anthropic", expired("sk-ant-oat-old"));
    let mut models = Models::new()
        .with_env(Arc::new(|_| None))
        .with_credentials(store);
    models.set_provider(Arc::new(
        HttpProvider::new("anthropic", "A", Vec::<String>::new(), vec![model.clone()])
            .with_oauth(Arc::new(Failing)),
    ));
    let message = models
        .complete(&model, &context(), StreamOptions::default())
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.unwrap();
    assert!(
        error.contains("invalid_grant") && error.contains("sign in again"),
        "{error}"
    );
}

const RESPONSES_SSE: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hi\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\"}]}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n";

#[tokio::test]
async fn codex_requests_carry_the_account() {
    let (url, recorded) = serve(vec![Reply::sse(RESPONSES_SSE)]).await;
    let model = model("openai-codex-responses", "openai-codex", &url);
    let (models, store, _flow) = setup(&model, "");
    let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acc-9"}});
    let token = format!(
        "h.{}.s",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    store.set_credential(
        "openai-codex",
        Credential::OAuth(OAuthCredential::new(token.clone(), "r", 3600.0, 0)),
    );
    let message = models
        .complete(
            &model,
            &context(),
            StreamOptions {
                session_id: Some("s-1".into()),
                ..StreamOptions::default()
            },
        )
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(message.text(), "Hi");

    let request = recorded.lock().unwrap()[0].clone();
    assert_eq!(request.request_line, "POST /codex/responses HTTP/1.1");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(request.header("chatgpt-account-id"), Some("acc-9"));
    assert_eq!(request.header("session-id"), Some("s-1"));
    assert_eq!(request.body["instructions"], "sys");
    assert_eq!(request.body["store"], false);
}

#[tokio::test]
async fn openai_responses_with_an_api_key() {
    let (url, recorded) = serve(vec![Reply::sse(RESPONSES_SSE)]).await;
    let model = model("openai-responses", "openai", &format!("{url}/v1"));
    let (models, store, _flow) = setup(&model, "");
    store.set("openai", "sk-test");
    let message = models
        .complete(&model, &context(), StreamOptions::default())
        .await;
    assert_eq!(message.text(), "Hi", "{:?}", message.error_message);
    assert_eq!(message.usage.input, 5);
    let request = recorded.lock().unwrap()[0].clone();
    assert_eq!(request.request_line, "POST /v1/responses HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(request.body["input"][0]["role"], "developer");
}

#[tokio::test]
async fn login_stores_the_credential() {
    let model = model("openai-responses", "openai", "http://127.0.0.1:9");
    let (models, store, _flow) = setup(&model, "tok-");
    let (interaction, _input) =
        LoginInteraction::new(CancellationToken::new(), |_| {}, Default::default());
    models.login("openai", interaction).await.unwrap();
    assert!(models.is_signed_in("openai"));
    assert!(
        matches!(store.credential("openai"), Some(Credential::OAuth(c)) if c.access == "tok-login")
    );
    assert!(
        models
            .login(
                "nope",
                LoginInteraction::new(CancellationToken::new(), |_| {}, Default::default()).0
            )
            .await
            .is_err()
    );
}
