//! Wire protocol implementations.

pub(crate) mod anthropic;
pub(crate) mod openai_completions;
pub(crate) mod openai_responses;

use serde_json::{Map, Value};

use crate::model::{Model, ThinkingLevel};

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
