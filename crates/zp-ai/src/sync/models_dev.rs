//! The <https://models.dev> catalog: fetching, lenient parsing and conversion helpers.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::SyncError;
use crate::http;
use crate::model::{CostTier, InputModality, ModelCost, ThinkingLevel};

/// Public models.dev catalog endpoint.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Retries for catalog fetches (separate from chat requests).
const MAX_RETRIES: u32 = 2;

/// The subset of the models.dev catalog ZP uses, keyed by models.dev provider id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevCatalog {
    /// Providers by models.dev id (e.g. `zai-coding-plan`).
    pub providers: BTreeMap<String, ModelsDevProvider>,
}

/// One models.dev provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevProvider {
    /// models.dev provider id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// OpenAI-compatible base URL, when models.dev knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Environment variables models.dev associates with the provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// Models by id.
    #[serde(default)]
    pub models: BTreeMap<String, ModelsDevModel>,
}

/// One models.dev model. Unknown fields are ignored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevModel {
    /// Model id as sent to the provider.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Model family, e.g. `claude-sonnet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Non-chat kinds such as `embedding` or `speech`; absent for chat models.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Accepts file attachments.
    #[serde(default)]
    pub attachment: bool,
    /// Supports reasoning.
    #[serde(default)]
    pub reasoning: bool,
    /// How reasoning can be configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_options: Vec<ModelsDevReasoningOption>,
    /// Supports tool calls.
    #[serde(default)]
    pub tool_call: bool,
    /// Input and output modalities.
    #[serde(default)]
    pub modalities: ModelsDevModalities,
    /// Token limits.
    #[serde(default)]
    pub limit: ModelsDevLimits,
    /// Pricing per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelsDevCost>,
    /// `deprecated`, `beta`, `alpha`, or absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Release date (`YYYY-MM-DD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
}

/// A way to configure reasoning, e.g. `{"type": "effort", "values": ["low", "high"]}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevReasoningOption {
    /// `effort`, `toggle` or `budget_tokens`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Allowed effort values.
    #[serde(
        default,
        deserialize_with = "strings_only",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub values: Vec<String>,
}

/// Input and output modalities (`text`, `image`, `pdf`, `audio`, …).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevModalities {
    /// Accepted inputs.
    #[serde(default, deserialize_with = "strings_only")]
    pub input: Vec<String>,
    /// Produced outputs.
    #[serde(default, deserialize_with = "strings_only")]
    pub output: Vec<String>,
}

/// Token limits; zero when unknown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevLimits {
    /// Context window.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub context: u64,
    /// Output tokens.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub output: u64,
}

/// Base rates per million tokens.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevRates {
    /// Input.
    #[serde(default)]
    pub input: f64,
    /// Output.
    #[serde(default)]
    pub output: f64,
    /// Cache reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Pricing with optional context tiers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevCost {
    /// Base rates.
    #[serde(flatten)]
    pub base: ModelsDevRates,
    /// Rates that apply above a context size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<ModelsDevCostTier>,
    /// Legacy form of a single tier above 200k tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<ModelsDevRates>,
}

/// One pricing tier.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevCostTier {
    /// Rates in this tier.
    #[serde(flatten)]
    pub rates: ModelsDevRates,
    /// When the tier applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelsDevTierSpec>,
}

/// Tier condition, e.g. `{"type": "context", "size": 200000}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevTierSpec {
    /// Only `context` tiers are used.
    #[serde(rename = "type")]
    pub kind: String,
    /// Prompt size threshold in tokens.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub size: u64,
}

fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = Option::<f64>::deserialize(deserializer)?;
    Ok(value
        .filter(|v| v.is_finite() && *v > 0.0)
        .map_or(0, |v| v as u64))
}

fn strings_only<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let values = Option::<Vec<Value>>::deserialize(deserializer)?;
    Ok(values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect())
}

impl ModelsDevCatalog {
    /// Parses `api.json`, keeping only providers for which `keep` returns true. Entries
    /// that do not match the expected shape are skipped rather than failing the catalog.
    pub fn parse(json: &[u8], keep: impl Fn(&str) -> bool) -> Result<Self, String> {
        let raw: BTreeMap<String, Value> =
            serde_json::from_slice(json).map_err(|error| format!("invalid catalog: {error}"))?;
        let mut providers = BTreeMap::new();
        for (id, provider) in raw {
            if !keep(&id) {
                continue;
            }
            let Value::Object(mut fields) = provider else {
                continue;
            };
            let models = fields.remove("models");
            let mut parsed: ModelsDevProvider = match serde_json::from_value(fields.into()) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::warn!(provider = id, %error, "skipping models.dev provider");
                    continue;
                }
            };
            if parsed.id.is_empty() {
                parsed.id.clone_from(&id);
            }
            if let Some(Value::Object(models)) = models {
                for (key, model) in models {
                    match serde_json::from_value::<ModelsDevModel>(model) {
                        Ok(mut model) => {
                            if model.id.is_empty() {
                                model.id = key;
                            }
                            parsed.models.insert(model.id.clone(), model);
                        }
                        Err(error) => {
                            tracing::debug!(provider = id, model = key, %error, "skipping models.dev model");
                        }
                    }
                }
            }
            providers.insert(id, parsed);
        }
        Ok(Self { providers })
    }

    /// The provider with models.dev id `id`.
    pub fn provider(&self, id: &str) -> Option<&ModelsDevProvider> {
        self.providers.get(id)
    }

    /// Total number of models across providers.
    pub fn model_count(&self) -> usize {
        self.providers.values().map(|p| p.models.len()).sum()
    }
}

impl ModelsDevModel {
    /// Marked deprecated by models.dev.
    pub fn is_deprecated(&self) -> bool {
        self.status.as_deref() == Some("deprecated")
    }

    /// A chat model producing text (not an embedding, speech, image… model).
    pub fn is_chat(&self) -> bool {
        self.kind.as_deref().is_none_or(|kind| kind == "chat")
            && (self.modalities.output.is_empty()
                || self.modalities.output.iter().any(|m| m == "text"))
    }

    /// Whether ZP should offer the model even when the provider cannot list its models:
    /// a current chat model with tool calling (required for a coding agent).
    pub fn is_offered(&self) -> bool {
        self.is_chat() && self.tool_call && !self.is_deprecated()
    }

    /// Input kinds ZP can send.
    pub fn input_modalities(&self) -> Vec<InputModality> {
        let mut input = vec![InputModality::Text];
        if self.modalities.input.iter().any(|m| m == "image") {
            input.push(InputModality::Image);
        }
        input
    }

    /// Pricing in ZP's shape.
    pub fn model_cost(&self) -> Option<ModelCost> {
        let cost = self.cost.as_ref()?;
        let tier = |above: u64, rates: &ModelsDevRates| CostTier {
            input_tokens_above: above,
            input: rates.input,
            output: rates.output,
            cache_read: rates.cache_read.unwrap_or_default(),
            cache_write: rates.cache_write.unwrap_or_default(),
        };
        let mut tiers: Vec<CostTier> = cost
            .tiers
            .iter()
            .filter_map(|t| {
                let spec = t.tier.as_ref()?;
                (spec.kind == "context" && spec.size > 0).then(|| tier(spec.size, &t.rates))
            })
            .collect();
        if tiers.is_empty()
            && let Some(rates) = &cost.context_over_200k
        {
            tiers.push(tier(200_000, rates));
        }
        tiers.sort_by_key(|t| t.input_tokens_above);
        Some(ModelCost {
            input: cost.base.input,
            output: cost.base.output,
            cache_read: cost.base.cache_read.unwrap_or_default(),
            cache_write: cost.base.cache_write.unwrap_or_default(),
            tiers,
        })
    }

    /// A thinking-level map derived from the declared effort values, or `None` when the
    /// model does not declare any (the default mapping then applies).
    pub fn thinking_level_map(&self) -> Option<BTreeMap<ThinkingLevel, Option<String>>> {
        if !self.reasoning {
            return None;
        }
        let values = &self
            .reasoning_options
            .iter()
            .find(|option| option.kind == "effort" && !option.values.is_empty())?
            .values;
        let has = |value: &str| values.iter().any(|v| v == value);
        let mut map: BTreeMap<ThinkingLevel, Option<String>> = ThinkingLevel::ALL[1..]
            .iter()
            .map(|&level| {
                (
                    level,
                    has(level.as_str()).then(|| level.as_str().to_owned()),
                )
            })
            .collect();
        let can_disable = self
            .reasoning_options
            .iter()
            .any(|option| matches!(option.kind.as_str(), "toggle" | "budget_tokens"));
        if has("none") {
            map.insert(ThinkingLevel::Off, Some("none".into()));
        } else if !can_disable {
            map.insert(ThinkingLevel::Off, None);
        }
        Some(map)
    }
}

/// Result of a conditional catalog fetch.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelsDevFetch {
    /// The server confirmed the cached copy (matching ETag) is current.
    NotModified,
    /// A new catalog.
    Updated {
        /// Parsed catalog, limited to the requested providers.
        catalog: ModelsDevCatalog,
        /// ETag to send next time.
        etag: Option<String>,
    },
}

/// Fetches the catalog from `url`, keeping providers for which `keep` returns true. With
/// `etag`, an unchanged catalog yields [`ModelsDevFetch::NotModified`].
pub async fn fetch_models_dev(
    url: &str,
    etag: Option<&str>,
    keep: impl Fn(&str) -> bool,
    cancel: &CancellationToken,
) -> Result<ModelsDevFetch, SyncError> {
    let headers = http::header_map(etag.map(|etag| ("if-none-match", etag)));
    let response = http::get(url, headers, MAX_RETRIES, cancel).await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(ModelsDevFetch::NotModified);
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = tokio::select! {
        () = cancel.cancelled() => return Err(SyncError::Cancelled),
        body = response.bytes() => body.map_err(|error| SyncError::Failed(format!("reading catalog failed: {error}")))?,
    };
    let catalog = ModelsDevCatalog::parse(&body, keep).map_err(SyncError::Failed)?;
    Ok(ModelsDevFetch::Updated { catalog, etag })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "acme": {
                "id": "acme", "name": "Acme", "env": ["ACME_KEY"], "npm": "x",
                "api": "https://api.acme.test/v1",
                "models": {
                    "acme-2": {
                        "id": "acme-2", "name": "Acme 2", "family": "acme",
                        "attachment": true, "reasoning": true, "tool_call": true,
                        "reasoning_options": [{"type": "effort", "values": ["low", "high", "max"]}],
                        "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]},
                        "limit": {"context": 200000, "output": 32000.0},
                        "cost": {
                            "input": 1, "output": 4, "cache_read": 0.1, "reasoning": 4,
                            "tiers": [{"input": 2, "output": 8, "tier": {"type": "context", "size": 100000}}]
                        },
                        "release_date": "2026-01-01"
                    },
                    "acme-embed": {
                        "id": "acme-embed", "name": "Embed", "type": "embedding",
                        "tool_call": false, "modalities": {"input": ["text"], "output": ["embedding"]},
                        "limit": {"context": 8000, "output": 0}
                    },
                    "broken": {"id": "broken", "limit": "nope"}
                }
            },
            "other": {"id": "other", "models": {}}
        }))
        .unwrap()
    }

    #[test]
    fn parses_leniently_and_filters_providers() {
        let catalog = ModelsDevCatalog::parse(&sample(), |id| id == "acme").unwrap();
        assert_eq!(catalog.providers.len(), 1);
        let acme = catalog.provider("acme").unwrap();
        assert_eq!(acme.api.as_deref(), Some("https://api.acme.test/v1"));
        assert_eq!(acme.models.len(), 2, "the malformed model is skipped");

        let model = &acme.models["acme-2"];
        assert!(model.is_offered());
        assert_eq!(model.limit.output, 32_000);
        assert_eq!(
            model.input_modalities(),
            [InputModality::Text, InputModality::Image]
        );
        let cost = model.model_cost().unwrap();
        assert_eq!((cost.input, cost.output, cost.cache_read), (1.0, 4.0, 0.1));
        assert_eq!(cost.tiers[0].input_tokens_above, 100_000);
        assert_eq!(cost.tiers[0].output, 8.0);

        let map = model.thinking_level_map().unwrap();
        assert_eq!(map[&ThinkingLevel::Off], None);
        assert_eq!(map[&ThinkingLevel::Medium], None);
        assert_eq!(map[&ThinkingLevel::Max].as_deref(), Some("max"));

        let embed = &acme.models["acme-embed"];
        assert!(!embed.is_chat());
        assert!(!embed.is_offered());

        // The trimmed catalog round-trips through its own serialization (used for caching).
        let json = serde_json::to_vec(&catalog).unwrap();
        let back: ModelsDevCatalog = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, catalog);
    }

    #[test]
    fn legacy_200k_tier_and_toggleable_reasoning() {
        let model: ModelsDevModel = serde_json::from_value(json!({
            "id": "m", "reasoning": true,
            "reasoning_options": [{"type": "toggle"}, {"type": "effort", "values": ["none", "low", 3]}],
            "cost": {"input": 1, "output": 2, "context_over_200k": {"input": 2, "output": 4}}
        }))
        .unwrap();
        assert_eq!(
            model.model_cost().unwrap().tiers[0].input_tokens_above,
            200_000
        );
        let map = model.thinking_level_map().unwrap();
        assert_eq!(map[&ThinkingLevel::Off].as_deref(), Some("none"));
        assert_eq!(map[&ThinkingLevel::Low].as_deref(), Some("low"));
    }
}
