//! Model catalog sync against a local mock HTTP server.

mod common;

use std::sync::Arc;

use common::{Reply, serve};
use serde_json::json;
use zp_ai::{
    Api, CancellationToken, HttpProvider, Models, ModelsDevFetch, SyncError, UpstreamModels,
};

fn models_with(provider: HttpProvider) -> Models {
    let mut models = Models::new().with_env(Arc::new(|name| {
        (name == "MOCK_KEY").then(|| "secret".to_owned())
    }));
    models.set_provider(Arc::new(provider));
    models
}

#[tokio::test]
async fn lists_anthropic_models_across_pages() {
    let first = Reply::json(&json!({
        "data": [{"type": "model", "id": "claude-a", "display_name": "Claude A"}],
        "has_more": true,
        "last_id": "claude-a"
    }));
    let second = Reply::json(&json!({
        "data": [{"type": "model", "id": "claude-b", "display_name": "Claude B"}],
        "has_more": false,
        "last_id": "claude-b"
    }));
    let (url, recorded) = serve(vec![first, second]).await;
    let provider = HttpProvider::new("mock", "Mock", ["MOCK_KEY"], Vec::new())
        .with_api_endpoint(Api::AnthropicMessages, &url);
    let models = models_with(provider);

    let listed = models
        .fetch_upstream("mock", &CancellationToken::new())
        .await
        .unwrap();
    let ids: Vec<_> = listed.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["claude-a", "claude-b"]);
    assert!(listed.fetched_at > 0);

    let requests = recorded.lock().unwrap();
    assert_eq!(
        requests[0].request_line,
        "GET /v1/models?limit=1000 HTTP/1.1"
    );
    assert_eq!(
        requests[1].request_line,
        "GET /v1/models?limit=1000&after_id=claude-a HTTP/1.1"
    );
    assert_eq!(requests[0].header("x-api-key"), Some("secret"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    drop(requests);

    // Applying the list makes the listed models usable through the normal accessors.
    models.set_upstream("mock", Some(listed));
    let model = models.find("mock/claude-b").unwrap();
    assert_eq!(model.name, "Claude B");
    assert_eq!(model.api, Api::AnthropicMessages);
    assert_eq!(model.base_url, url);
}

#[tokio::test]
async fn lists_openai_compatible_models_and_reports_errors() {
    let ok = Reply::json(&json!({"object": "list", "data": [{"id": "m-1"}, {"id": "m-2"}]}));
    let denied = Reply::status(401, r#"{"error":{"message":"bad key"}}"#);
    let (url, recorded) = serve(vec![ok, denied]).await;
    let provider = HttpProvider::new("mock", "Mock", ["MOCK_KEY"], Vec::new())
        .with_api_endpoint(Api::OpenAiCompletions, format!("{url}/v1/"));
    let models = models_with(provider);
    let cancel = CancellationToken::new();

    let listed = models.fetch_upstream("mock", &cancel).await.unwrap();
    assert_eq!(listed.models.len(), 2);
    {
        let requests = recorded.lock().unwrap();
        assert_eq!(requests[0].request_line, "GET /v1/models HTTP/1.1");
        assert_eq!(requests[0].header("authorization"), Some("Bearer secret"));
    }

    assert_eq!(
        models.fetch_upstream("mock", &cancel).await,
        Err(SyncError::Failed("HTTP 401 Unauthorized: bad key".into()))
    );
}

#[tokio::test]
async fn models_dev_fetch_filters_providers_and_honors_etag() {
    let catalog = json!({
        "mock-dev": {"id": "mock-dev", "name": "Mock", "models": {
            "fresh": {"id": "fresh", "name": "Fresh", "tool_call": true,
                      "limit": {"context": 1000, "output": 100}}
        }},
        "unrelated": {"id": "unrelated", "models": {}}
    });
    let mut updated = Reply::json(&catalog);
    updated.headers.push(("etag", "\"v1\"".into()));
    let not_modified = Reply::status(304, "");
    let (url, recorded) = serve(vec![updated, not_modified]).await;

    let provider = HttpProvider::new("mock", "Mock", ["MOCK_KEY"], Vec::new())
        .with_api_endpoint(Api::OpenAiCompletions, "https://mock.test/v1")
        .with_models_dev_id("mock-dev");
    let models = models_with(provider);
    let cancel = CancellationToken::new();

    let ModelsDevFetch::Updated { catalog, etag } = models
        .fetch_models_dev(&format!("{url}/api.json"), None, &cancel)
        .await
        .unwrap()
    else {
        panic!("expected a catalog")
    };
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    assert_eq!(catalog.providers.keys().collect::<Vec<_>>(), ["mock-dev"]);

    let again = models
        .fetch_models_dev(&format!("{url}/api.json"), etag.as_deref(), &cancel)
        .await
        .unwrap();
    assert_eq!(again, ModelsDevFetch::NotModified);
    assert_eq!(
        recorded.lock().unwrap()[1].header("if-none-match"),
        Some("\"v1\"")
    );

    models.set_models_dev(Some(catalog));
    let fresh = models.find("mock/fresh").unwrap();
    assert_eq!(fresh.base_url, "https://mock.test/v1");
    assert_eq!(fresh.context_window, 1000);

    // The upstream layer can hide what models.dev added.
    models.set_upstream(
        "mock",
        Some(UpstreamModels::new(vec![zp_ai::UpstreamModel::new(
            "other",
        )])),
    );
    assert!(models.find("mock/fresh").is_none());
    assert!(models.find("mock/other").is_some());
}
