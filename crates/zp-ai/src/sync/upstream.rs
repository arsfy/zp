//! Listing the models a provider currently offers (`GET /v1/models`, `GET /models`).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::SyncError;
use crate::http;
use crate::model::{Api, InputModality, ModelCost};

/// Retries for list requests (separate from chat requests).
const MAX_RETRIES: u32 = 2;
/// Anthropic page size (the API maximum).
const ANTHROPIC_PAGE_SIZE: &str = "1000";
/// Guard against servers that keep returning `has_more`.
const MAX_PAGES: usize = 20;
/// Id fragments of models that are not chat models. Only consulted for models that
/// neither the catalog nor models.dev describe.
const NON_CHAT_HINTS: &[&str] = &[
    "embed",
    "whisper",
    "tts",
    "transcribe",
    "moderation",
    "rerank",
    "guard",
    "dall-e",
    "image",
    "audio",
    "speech",
    "orpheus",
    "realtime",
];

/// A model as reported by the provider. Optional fields are only present when the API
/// returns them (OpenRouter and Groq report limits; OpenRouter also pricing).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamModel {
    /// Model id.
    pub id: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Output token limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Accepted inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<InputModality>>,
    /// Whether the model produces text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_output: Option<bool>,
    /// Whether the model accepts tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    /// Whether the model supports reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Pricing per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
}

impl UpstreamModel {
    /// A model with only an id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// Best-effort guess that this is a tool-capable chat model.
    pub fn looks_like_chat(&self) -> bool {
        if self.tools == Some(false) || self.text_output == Some(false) {
            return false;
        }
        let id = self.id.to_ascii_lowercase();
        !NON_CHAT_HINTS.iter().any(|hint| id.contains(hint))
    }
}

/// A provider's model list at a point in time.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamModels {
    /// When the list was fetched (Unix milliseconds).
    pub fetched_at: i64,
    /// Listed models.
    pub models: Vec<UpstreamModel>,
}

impl UpstreamModels {
    /// A list fetched now.
    pub fn new(models: Vec<UpstreamModel>) -> Self {
        Self {
            fetched_at: crate::types::now_ms(),
            models,
        }
    }

    /// Whether `id` is listed.
    pub fn contains(&self, id: &str) -> bool {
        self.models.iter().any(|model| model.id == id)
    }
}

/// Lists the models served at `base_url` with protocol `api`.
pub async fn list_upstream_models(
    api: Api,
    base_url: &str,
    api_key: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<UpstreamModel>, SyncError> {
    let base_url = base_url.trim_end_matches('/');
    let models = match api {
        Api::AnthropicMessages => list_anthropic(base_url, api_key, cancel).await?,
        Api::OpenAiCompletions | Api::OpenAiResponses => {
            list_openai(base_url, api_key, cancel).await?
        }
        Api::OpenAiCodexResponses | Api::Faux => return Err(SyncError::Unsupported),
    };
    let mut seen = HashSet::new();
    Ok(models
        .into_iter()
        .filter(|model| seen.insert(model.id.clone()))
        .collect())
}

async fn list_anthropic(
    base_url: &str,
    api_key: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<UpstreamModel>, SyncError> {
    let mut headers = vec![("anthropic-version", crate::api::anthropic::API_VERSION)];
    let bearer = api_key
        .filter(|key| crate::api::anthropic::is_oauth_token(key))
        .map(|key| format!("Bearer {key}"));
    match (&bearer, api_key) {
        (Some(bearer), _) => {
            headers.push(("authorization", bearer.as_str()));
            headers.push(("anthropic-beta", crate::api::anthropic::OAUTH_BETAS));
        }
        (None, Some(key)) => headers.push(("x-api-key", key)),
        (None, None) => {}
    }
    let headers = http::header_map(headers);
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut url = reqwest::Url::parse(&format!("{base_url}/v1/models"))
            .map_err(|error| SyncError::Failed(format!("invalid base URL: {error}")))?;
        url.query_pairs_mut()
            .append_pair("limit", ANTHROPIC_PAGE_SIZE);
        if let Some(after) = &after {
            url.query_pairs_mut().append_pair("after_id", after);
        }
        let response = http::get(url.as_str(), headers.clone(), MAX_RETRIES, cancel).await?;
        let page = http::read_json(response, cancel).await?;
        let (entries, next) = parse_anthropic_page(&page);
        models.extend(entries);
        match next {
            Some(next) if after.as_deref() != Some(next.as_str()) => after = Some(next),
            _ => break,
        }
    }
    Ok(models)
}

async fn list_openai(
    base_url: &str,
    api_key: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<UpstreamModel>, SyncError> {
    let authorization = api_key.map(|key| format!("Bearer {key}"));
    let headers = http::header_map(
        authorization
            .as_deref()
            .map(|value| ("authorization", value)),
    );
    let response = http::get(&format!("{base_url}/models"), headers, MAX_RETRIES, cancel).await?;
    let body = http::read_json(response, cancel).await?;
    Ok(parse_openai_list(&body))
}

/// Parses one page of `GET /v1/models`, returning the models and the cursor for the next
/// page if there is one.
pub(crate) fn parse_anthropic_page(page: &Value) -> (Vec<UpstreamModel>, Option<String>) {
    let models = page
        .get("data")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let mut model = UpstreamModel::new(entry_id(entry)?);
                    model.name = string(entry.get("display_name"));
                    model.context_window = count(entry.get("max_input_tokens"));
                    model.max_tokens = count(entry.get("max_tokens"));
                    Some(model)
                })
                .collect()
        })
        .unwrap_or_default();
    let has_more = page.get("has_more").and_then(Value::as_bool) == Some(true);
    let next = has_more.then(|| string(page.get("last_id"))).flatten();
    (models, next)
}

/// Parses `GET /models` from OpenAI-compatible servers (`{"data": [...]}` or a bare array).
pub(crate) fn parse_openai_list(body: &Value) -> Vec<UpstreamModel> {
    let entries = body
        .get("data")
        .or_else(|| body.get("models"))
        .unwrap_or(body)
        .as_array();
    entries
        .map(|entries| entries.iter().filter_map(parse_openai_entry).collect())
        .unwrap_or_default()
}

fn parse_openai_entry(entry: &Value) -> Option<UpstreamModel> {
    let mut model = UpstreamModel::new(entry_id(entry)?);
    if entry.get("active").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let top = entry.get("top_provider");
    let architecture = entry.get("architecture");
    let field = |name: &str| entry.get(name);

    model.name = string(field("name")).or_else(|| string(field("display_name")));
    model.context_window = count(field("context_length"))
        .or_else(|| count(field("context_window")))
        .or_else(|| count(field("max_context_length")))
        .or_else(|| count(top.and_then(|t| t.get("context_length"))));
    model.max_tokens = count(field("max_completion_tokens"))
        .or_else(|| count(top.and_then(|t| t.get("max_completion_tokens"))))
        .or_else(|| count(field("max_output_tokens")));

    let input = strings(architecture.and_then(|a| a.get("input_modalities")))
        .or_else(|| strings(field("input_modalities")));
    model.input = input.map(|input| {
        let mut modalities = vec![InputModality::Text];
        if input.iter().any(|m| m == "image") {
            modalities.push(InputModality::Image);
        }
        modalities
    });
    model.text_output = strings(architecture.and_then(|a| a.get("output_modalities")))
        .map(|output| output.iter().any(|m| m == "text"));

    if let Some(parameters) = strings(field("supported_parameters")) {
        let has = |name: &str| parameters.iter().any(|p| p == name);
        model.tools = Some(has("tools"));
        model.reasoning = Some(has("reasoning") || has("include_reasoning"));
    }

    if let Some(pricing) = field("pricing") {
        let per_million = |name: &str| {
            let value = pricing.get(name)?;
            let rate = value
                .as_f64()
                .or_else(|| value.as_str()?.trim().parse().ok())?;
            (rate.is_finite() && rate >= 0.0).then_some(rate * 1_000_000.0)
        };
        if let (Some(input), Some(output)) = (per_million("prompt"), per_million("completion")) {
            model.cost = Some(ModelCost {
                input,
                output,
                cache_read: per_million("input_cache_read").unwrap_or_default(),
                cache_write: per_million("input_cache_write").unwrap_or_default(),
                tiers: Vec::new(),
            });
        }
    }
    Some(model)
}

fn entry_id(entry: &Value) -> Option<String> {
    string(entry.get("id"))
}

fn string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
    )
}

fn count(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| v as u64)
        })
        .filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_anthropic_pages() {
        let (models, next) = parse_anthropic_page(&json!({
            "data": [
                {"type": "model", "id": "claude-a-20260101", "display_name": "Claude A", "created_at": "2026-01-01T00:00:00Z"},
                {"type": "model", "id": "claude-b", "display_name": "Claude B", "max_input_tokens": 200000, "max_tokens": 64000},
                {"type": "model"}
            ],
            "has_more": true,
            "first_id": "claude-a-20260101",
            "last_id": "claude-b"
        }));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name.as_deref(), Some("Claude A"));
        assert_eq!(models[1].context_window, Some(200_000));
        assert_eq!(models[1].max_tokens, Some(64_000));
        assert_eq!(next.as_deref(), Some("claude-b"));

        let (_, next) =
            parse_anthropic_page(&json!({"data": [], "has_more": false, "last_id": "x"}));
        assert_eq!(next, None);
    }

    #[test]
    fn parses_openai_compatible_lists() {
        let models = parse_openai_list(&json!({
            "object": "list",
            "data": [
                {"id": "plain", "object": "model", "owned_by": "acme"},
                {"id": "groq-model", "context_window": 131072, "max_completion_tokens": 32768, "active": true},
                {"id": "retired", "active": false},
                {
                    "id": "vendor/router-model", "name": "Vendor: Model",
                    "context_length": 262144,
                    "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
                    "top_provider": {"max_completion_tokens": 65536},
                    "pricing": {"prompt": "0.0000005", "completion": "0.000002", "input_cache_read": "0.00000005"},
                    "supported_parameters": ["tools", "reasoning", "max_tokens"]
                },
                {"id": "openrouter/auto", "pricing": {"prompt": "-1", "completion": "-1"}, "supported_parameters": ["max_tokens"]}
            ]
        }));
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "plain",
                "groq-model",
                "vendor/router-model",
                "openrouter/auto"
            ]
        );
        assert_eq!(models[1].context_window, Some(131_072));
        assert_eq!(models[1].max_tokens, Some(32_768));

        let routed = &models[2];
        assert_eq!(routed.name.as_deref(), Some("Vendor: Model"));
        assert_eq!(routed.max_tokens, Some(65_536));
        assert_eq!(
            routed.input.as_deref(),
            Some(&[InputModality::Text, InputModality::Image][..])
        );
        assert_eq!((routed.tools, routed.reasoning), (Some(true), Some(true)));
        let cost = routed.cost.as_ref().unwrap();
        assert!((cost.input - 0.5).abs() < 1e-9 && (cost.output - 2.0).abs() < 1e-9);
        assert!((cost.cache_read - 0.05).abs() < 1e-9);

        assert!(models[3].cost.is_none(), "negative prices are placeholders");
        assert!(!models[3].looks_like_chat(), "no tool support");

        // Bare arrays work too.
        assert_eq!(parse_openai_list(&json!([{"id": "x"}])).len(), 1);
    }

    #[test]
    fn non_chat_heuristics() {
        assert!(UpstreamModel::new("llama-4-70b").looks_like_chat());
        for id in [
            "text-embedding-3-large",
            "whisper-large-v3",
            "playai-tts",
            "llama-guard-4",
        ] {
            assert!(!UpstreamModel::new(id).looks_like_chat(), "{id}");
        }
    }
}
