//! End-to-end tests against a local mock HTTP server.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Reply, serve};
use serde_json::json;
use zp_ai::{
    AssistantMessageEvent, Context, HttpProvider, Message, Model, Models, StopReason, StreamOptions,
};

fn model(api: &str, base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": "mock-model",
        "name": "Mock",
        "api": api,
        "provider": "mock",
        "baseUrl": base_url,
        "input": ["text"],
        "cost": {"input": 1, "output": 2},
        "contextWindow": 100000,
        "maxTokens": 1000
    }))
    .unwrap()
}

fn models(model: &Model) -> Models {
    let mut models = Models::new().with_env(Arc::new(|name| {
        (name == "MOCK_KEY").then(|| "secret".to_owned())
    }));
    models.set_provider(Arc::new(HttpProvider::new(
        "mock",
        "Mock",
        ["MOCK_KEY"],
        vec![model.clone()],
    )));
    models
}

fn context() -> Context {
    Context {
        system_prompt: Some("sys".into()),
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
    }
}

const ANTHROPIC_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi!\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

#[tokio::test]
async fn anthropic_round_trip() {
    let (url, recorded) = serve(vec![Reply::sse(ANTHROPIC_SSE)]).await;
    let model = model("anthropic-messages", &url);
    let message = models(&model)
        .complete(&model, &context(), StreamOptions::default())
        .await;

    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(message.text(), "Hi!");
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 3);
    assert!((message.usage.cost.total - (10.0 + 6.0) / 1_000_000.0).abs() < 1e-12);

    let requests = recorded.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.request_line, "POST /v1/messages HTTP/1.1");
    assert_eq!(request.header("x-api-key"), Some("secret"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.body["model"], "mock-model");
    assert_eq!(request.body["system"][0]["text"], "sys");
}

#[tokio::test]
async fn openai_round_trip_with_chunked_writes() {
    let chunks = [
        "data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n",
        "\ndata: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
    ];
    let reply = Reply {
        parts: chunks.iter().map(|c| (*c).to_owned()).collect(),
        pause: Duration::from_millis(10),
        ..Reply::sse("")
    };
    let (url, recorded) = serve(vec![reply]).await;
    let model = model("openai-completions", &format!("{url}/v1"));
    let mut stream = models(&model).stream(&model, &context(), StreamOptions::default());

    let mut deltas = Vec::new();
    let mut last = None;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        if let AssistantMessageEvent::TextDelta { delta, .. } = &event {
            deltas.push(delta.clone());
        }
        last = Some(event);
    }
    assert_eq!(deltas, ["Hel", "lo"]);
    let Some(AssistantMessageEvent::Done { message }) = last else {
        panic!("expected done: {last:?}")
    };
    assert_eq!(message.text(), "Hello");
    assert_eq!(message.usage.input, 7);

    let request = recorded.lock().unwrap()[0].clone();
    assert_eq!(request.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(request.header("authorization"), Some("Bearer secret"));
    assert_eq!(request.body["messages"][0]["role"], "system");
}

#[tokio::test]
async fn retries_rate_limits_then_succeeds() {
    let mut limited = Reply::status(429, r#"{"error":{"message":"slow down"}}"#);
    limited.headers.push(("retry-after", "0".into()));
    let (url, recorded) = serve(vec![limited, Reply::sse(ANTHROPIC_SSE)]).await;
    let model = model("anthropic-messages", &url);
    let message = models(&model)
        .complete(&model, &context(), StreamOptions::default())
        .await;
    assert_eq!(
        message.stop_reason,
        StopReason::Stop,
        "{:?}",
        message.error_message
    );
    assert_eq!(recorded.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let unauthorized = Reply::status(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    );
    let (url, recorded) = serve(vec![unauthorized, Reply::sse(ANTHROPIC_SSE)]).await;
    let model = model("anthropic-messages", &url);
    let message = models(&model)
        .complete(&model, &context(), StreamOptions::default())
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("HTTP 401 Unauthorized: invalid x-api-key")
    );
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn generic_proxy_errors_keep_details() {
    let failed = Reply::status(
        400,
        r#"{"error":{"message":"openai_error","type":"bad_response_status_code","param":"max_tokens"}}"#,
    );
    // Some proxies answer a streaming request with a JSON error and status 200.
    let ok_but_error = Reply::json(&json!({"error": {"message": "model not found: foo"}}));
    let (url, _) = serve(vec![failed, ok_but_error]).await;
    let model = model("openai-completions", &format!("{url}/v1"));

    let message = models(&model)
        .complete(&model, &context(), StreamOptions::default())
        .await;
    let error = message.error_message.unwrap();
    assert!(
        error.starts_with("HTTP 400 Bad Request: openai_error"),
        "{error}"
    );
    assert!(error.contains("max_tokens"), "{error}");

    let message = models(&model)
        .complete(&model, &context(), StreamOptions::default())
        .await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("model not found: foo")
    );
}

#[tokio::test]
async fn cancellation_mid_stream_aborts() {
    let reply = Reply {
        parts: vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".into(),
            "data: {\"choices\":[{\"delta\":{\"content\":\" never\"},\"finish_reason\":\"stop\"}]}\n\n".into(),
        ],
        pause: Duration::from_secs(5),
        ..Reply::sse("")
    };
    let (url, _) = serve(vec![reply]).await;
    let model = model("openai-completions", &url);
    let options = StreamOptions::default();
    let cancel = options.cancel.clone();
    let mut stream = models(&model).stream(&model, &context(), options);
    let started = std::time::Instant::now();
    let mut last = None;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
            cancel.cancel();
        }
        last = Some(event);
    }
    assert!(started.elapsed() < Duration::from_secs(4));
    let Some(AssistantMessageEvent::Error { message }) = last else {
        panic!("expected error")
    };
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.text(), "partial");
}
