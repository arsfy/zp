//! Wire protocol implementations.

pub(crate) mod anthropic;
pub(crate) mod openai_completions;
pub(crate) mod openai_responses;

use serde_json::{Map, Value};

use crate::model::{Api, Model, PromptCacheKeyMode, ThinkingLevel};
use crate::options::{CacheRetention, StreamOptions};

// OpenAI documents 24h retention for these model families on the legacy API.
const EXTENDED_CACHE_MODELS: &[&str] = &[
    "gpt-5.5",
    "gpt-5.5-pro",
    "gpt-5.4",
    "gpt-5.2",
    "gpt-5.1-codex-max",
    "gpt-5.1",
    "gpt-5.1-codex",
    "gpt-5.1-codex-mini",
    "gpt-5.1-chat-latest",
    "gpt-5",
    "gpt-5-codex",
    "gpt-4.1",
];

/// Adds the cache controls that this endpoint supports.
fn add_openai_cache_options(
    body: &mut Map<String, Value>,
    model: &Model,
    options: &StreamOptions,
    use_session_key_by_default: bool,
) {
    let official = reqwest::Url::parse(&model.base_url)
        .is_ok_and(|url| url.host_str() == Some("api.openai.com"));
    let version = model.id.strip_prefix("gpt-").and_then(|id| {
        let (major, minor) = id.split_once('.')?;
        let minor = minor.split('-').next()?;
        Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?))
    });
    let modern = official && version.is_some_and(|version| version >= (5, 6));
    let explicit = model.api == Api::OpenAiResponses
        && model
            .compat
            .supports_explicit_prompt_cache_mode
            .unwrap_or(modern);
    if options.cache_retention == CacheRetention::None {
        if explicit {
            body.insert(
                "prompt_cache_options".into(),
                serde_json::json!({"mode": "explicit"}),
            );
        }
        return;
    }
    let supports_long = model
        .compat
        .supports_long_cache_retention
        .unwrap_or_else(|| {
            explicit
                || (official
                    && EXTENDED_CACHE_MODELS.iter().any(|name| {
                        model.id == *name
                            || model
                                .id
                                .strip_prefix(name)
                                .is_some_and(|suffix| suffix.starts_with("-202"))
                    }))
        });
    let long = options.cache_retention == CacheRetention::Long && supports_long;
    let send_key = match model
        .compat
        .prompt_cache_key_mode
        .unwrap_or(PromptCacheKeyMode::Auto)
    {
        PromptCacheKeyMode::Auto => use_session_key_by_default || official || long,
        PromptCacheKeyMode::Enabled => true,
        PromptCacheKeyMode::Disabled => false,
    };
    if send_key && let Some(session) = options.session_id.as_deref() {
        let key: String = session.chars().take(64).collect();
        body.insert("prompt_cache_key".into(), Value::from(key));
    }
    if long && explicit {
        body.insert(
            "prompt_cache_options".into(),
            serde_json::json!({"ttl": "12h"}),
        );
    } else if long {
        body.insert("prompt_cache_retention".into(), Value::from("24h"));
    }
}

/// Per-request headers GitHub Copilot expects (who initiated the request, images).
pub(crate) fn github_copilot_headers(
    context: &crate::types::Context,
) -> Vec<(&'static str, &'static str)> {
    crate::oauth::copilot_dynamic_headers(context)
}

/// The provider value for a (supported) thinking level: the model's mapping or the level
/// name itself.
pub(crate) fn mapped_level(model: &Model, level: ThinkingLevel) -> String {
    match model.thinking_level_value(level) {
        Some(Some(value)) => value.to_owned(),
        _ => level.as_str().to_owned(),
    }
}

/// Normalizes a tool parameter schema to an object schema with `properties` and
/// `required`, which every provider accepts.
pub(crate) fn object_schema(parameters: &Value) -> Value {
    let mut schema = Map::new();
    schema.insert("type".into(), Value::from("object"));
    schema.insert(
        "properties".into(),
        parameters
            .get("properties")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())),
    );
    schema.insert(
        "required".into(),
        parameters
            .get("required")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    );
    for key in ["$defs", "definitions", "additionalProperties"] {
        if let Some(value) = parameters.get(key) {
            schema.insert(key.into(), value.clone());
        }
    }
    Value::Object(schema)
}

/// Parses complete tool arguments, falling back to partial parsing and finally `{}`.
pub(crate) fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(raw)
        .ok()
        .or_else(|| crate::partial_json::parse_partial_json(raw))
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}
