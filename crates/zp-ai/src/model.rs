//! Model metadata, thinking levels and cost calculation.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::types::Usage;

/// Wire protocol used to talk to a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Api {
    /// Anthropic Messages API (`/v1/messages`).
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    /// OpenAI Chat Completions and compatible APIs (`/chat/completions`).
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    /// OpenAI Responses API (`/responses`).
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// The Responses API of ChatGPT's Codex backend (`/codex/responses`), used with a
    /// ChatGPT sign-in.
    #[serde(rename = "openai-codex-responses")]
    OpenAiCodexResponses,
    /// Scripted in-process provider for tests.
    #[serde(rename = "faux")]
    Faux,
}

impl Api {
    /// The protocol identifier, e.g. `anthropic-messages`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiCodexResponses => "openai-codex-responses",
            Self::Faux => "faux",
        }
    }
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Input kinds a model accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    /// Text input.
    Text,
    /// Image input.
    Image,
}

/// Reasoning effort requested from a model.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// No reasoning.
    #[default]
    Off,
    /// Minimal reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
    /// Extra-high reasoning (only on models that map it).
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum reasoning (only on models that map it).
    Max,
}

impl ThinkingLevel {
    /// All levels from lowest to highest.
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The identifier used in settings and on the wire by default.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(value))
            .ok_or_else(|| {
                format!("unknown thinking level `{value}` (expected off, minimal, low, medium, high, xhigh or max)")
            })
    }
}

/// Price per million tokens in US dollars.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// Uncached input.
    pub input: f64,
    /// Output.
    pub output: f64,
    /// Cache reads.
    #[serde(default)]
    pub cache_read: f64,
    /// Cache writes.
    #[serde(default)]
    pub cache_write: f64,
    /// Higher rates that apply once the prompt exceeds a size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<CostTier>,
}

/// Rates that apply when total input exceeds `input_tokens_above`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostTier {
    /// Threshold on total input tokens (cached and uncached).
    pub input_tokens_above: u64,
    /// Uncached input.
    pub input: f64,
    /// Output.
    pub output: f64,
    /// Cache reads.
    #[serde(default)]
    pub cache_read: f64,
    /// Cache writes.
    #[serde(default)]
    pub cache_write: f64,
}

/// Which request field carries the output token limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// Legacy `max_tokens`.
    MaxTokens,
    /// `max_completion_tokens`.
    MaxCompletionTokens,
}

/// How an OpenAI-compatible endpoint expects reasoning to be configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingFormat {
    /// OpenAI-style `reasoning_effort`.
    Openai,
    /// DeepSeek / Moonshot: `thinking: {type}` plus optional `reasoning_effort`.
    Deepseek,
    /// Z.ai: `thinking: {type, clear_thinking}`.
    Zai,
    /// OpenRouter: `reasoning: {effort}`.
    Openrouter,
    /// Qwen: `enable_thinking`.
    Qwen,
    /// Together: `reasoning: {enabled}`.
    Together,
    /// A format ZP does not implement; no reasoning parameters are sent.
    #[serde(other)]
    Other,
}

/// Per-model protocol quirks. Unset fields fall back to auto-detection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCompat {
    // --- anthropic-messages ---
    /// Use adaptive thinking with an effort level instead of a token budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_adaptive_thinking: Option<bool>,
    /// Whether `temperature` may be sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_temperature: Option<bool>,
    /// Whether tools may set `eager_input_streaming`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_eager_tool_input_streaming: Option<bool>,
    /// Whether thinking blocks with empty signatures can be replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_empty_signature: Option<bool>,

    // --- openai-completions ---
    /// Whether `store: false` may be sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    /// Whether reasoning models accept the `developer` role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    /// Whether `reasoning_effort` may be sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    /// Whether `stream_options.include_usage` may be sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    /// Field for the output token limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    /// Tool results must carry the tool `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_tool_result_name: Option<bool>,
    /// A user message may not directly follow tool results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_assistant_after_tool_result: Option<bool>,
    /// Replay reasoning as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thinking_as_text: Option<bool>,
    /// Assistant messages must carry `reasoning_content` (possibly empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    /// How reasoning is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    /// Whether tool definitions accept `strict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    /// Z.ai streaming tool arguments (`tool_stream: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zai_tool_stream: Option<bool>,
    /// Cache-control markers to add (`anthropic` for Claude behind OpenRouter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control_format: Option<String>,
}

/// Static description of a chat model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// Model id sent to the provider.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Wire protocol.
    pub api: Api,
    /// Owning provider id.
    pub provider: String,
    /// API base URL.
    pub base_url: String,
    /// Whether the model supports reasoning.
    #[serde(default)]
    pub reasoning: bool,
    /// Accepted input kinds.
    #[serde(default = "default_input")]
    pub input: Vec<InputModality>,
    /// Pricing.
    #[serde(default)]
    pub cost: ModelCost,
    /// Context window in tokens.
    #[serde(default)]
    pub context_window: u64,
    /// Default and maximum output tokens.
    #[serde(default)]
    pub max_tokens: u64,
    /// Provider value per thinking level. A level mapped to `null` is unsupported; an
    /// absent level uses the default mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
    /// Extra request headers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Protocol quirks.
    #[serde(default)]
    pub compat: ModelCompat,
}

fn default_input() -> Vec<InputModality> {
    vec![InputModality::Text]
}

impl Model {
    /// Whether the model accepts image input.
    pub fn supports_images(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }

    /// `provider/id`, the canonical way to reference a model.
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// The explicit mapping for `level`: `None` if absent, `Some(None)` if unsupported.
    pub fn thinking_level_value(&self, level: ThinkingLevel) -> Option<Option<&str>> {
        self.thinking_level_map
            .as_ref()?
            .get(&level)
            .map(|value| value.as_deref())
    }

    /// Thinking levels this model supports, lowest first.
    pub fn supported_thinking_levels(&self) -> Vec<ThinkingLevel> {
        if !self.reasoning {
            return vec![ThinkingLevel::Off];
        }
        ThinkingLevel::ALL
            .into_iter()
            .filter(|&level| match self.thinking_level_value(level) {
                Some(None) => false,
                Some(Some(_)) => true,
                None => !matches!(level, ThinkingLevel::XHigh | ThinkingLevel::Max),
            })
            .collect()
    }

    /// The closest supported level, preferring higher levels, then lower ones.
    pub fn clamp_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevel {
        let available = self.supported_thinking_levels();
        if available.contains(&level) {
            return level;
        }
        let index = ThinkingLevel::ALL
            .iter()
            .position(|&l| l == level)
            .unwrap_or(0);
        ThinkingLevel::ALL[index..]
            .iter()
            .chain(ThinkingLevel::ALL[..index].iter().rev())
            .copied()
            .find(|candidate| available.contains(candidate))
            .unwrap_or(ThinkingLevel::Off)
    }

    /// Fills `usage.cost` from the model's rates and returns the total.
    pub fn calculate_cost(&self, usage: &mut Usage) -> f64 {
        let total_input = usage.total_input();
        let (mut input, mut output, mut cache_read, mut cache_write) = (
            self.cost.input,
            self.cost.output,
            self.cost.cache_read,
            self.cost.cache_write,
        );
        let mut threshold = None;
        for tier in &self.cost.tiers {
            if total_input > tier.input_tokens_above
                && threshold.is_none_or(|t| tier.input_tokens_above > t)
            {
                (input, output, cache_read, cache_write) =
                    (tier.input, tier.output, tier.cache_read, tier.cache_write);
                threshold = Some(tier.input_tokens_above);
            }
        }
        let per_token = |rate: f64, tokens: u64| rate / 1_000_000.0 * tokens as f64;
        usage.cost.input = per_token(input, usage.input);
        usage.cost.output = per_token(output, usage.output);
        usage.cost.cache_read = per_token(cache_read, usage.cache_read);
        usage.cost.cache_write = per_token(cache_write, usage.cache_write);
        usage.cost.total =
            usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
        usage.cost.total
    }
}

#[cfg(test)]
pub(crate) fn test_model(api: Api) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api,
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: Vec::new(),
        },
        context_window: 200_000,
        max_tokens: 64_000,
        thinking_level_map: None,
        headers: HashMap::new(),
        compat: ModelCompat::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_catalog_entry_and_ignores_unknown_fields() {
        let model: Model = serde_json::from_value(json!({
            "id": "claude-x",
            "name": "Claude X",
            "api": "anthropic-messages",
            "provider": "anthropic",
            "baseUrl": "https://api.anthropic.com",
            "reasoning": true,
            "input": ["text", "image"],
            "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 1.25},
            "contextWindow": 1000,
            "maxTokens": 100,
            "thinkingLevelMap": {"off": null, "xhigh": "xhigh"},
            "compat": {"forceAdaptiveThinking": true, "thinkingFormat": "something-new"},
            "unknownField": 1
        }))
        .unwrap();
        assert_eq!(model.api, Api::AnthropicMessages);
        assert!(model.supports_images());
        assert_eq!(model.compat.force_adaptive_thinking, Some(true));
        assert_eq!(model.compat.thinking_format, Some(ThinkingFormat::Other));
        assert_eq!(model.thinking_level_value(ThinkingLevel::Off), Some(None));
        assert_eq!(
            model.thinking_level_value(ThinkingLevel::XHigh),
            Some(Some("xhigh"))
        );
        assert_eq!(model.thinking_level_value(ThinkingLevel::Low), None);
    }

    #[test]
    fn supported_levels_follow_the_map() {
        let mut model = test_model(Api::AnthropicMessages);
        assert_eq!(model.supported_thinking_levels(), vec![ThinkingLevel::Off]);

        model.reasoning = true;
        assert_eq!(
            model.supported_thinking_levels(),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High
            ]
        );

        model.thinking_level_map = Some(BTreeMap::from([
            (ThinkingLevel::Off, None),
            (ThinkingLevel::Minimal, None),
            (ThinkingLevel::Max, Some("max".into())),
        ]));
        assert_eq!(
            model.supported_thinking_levels(),
            vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Max
            ]
        );
        // Unsupported levels move up first, then down.
        assert_eq!(
            model.clamp_thinking_level(ThinkingLevel::Off),
            ThinkingLevel::Low
        );
        assert_eq!(
            model.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::Max
        );
        assert_eq!(
            model.clamp_thinking_level(ThinkingLevel::Medium),
            ThinkingLevel::Medium
        );
    }

    #[test]
    fn cost_uses_matching_tier() {
        let mut model = test_model(Api::AnthropicMessages);
        model.cost.tiers = vec![CostTier {
            input_tokens_above: 1_000,
            input: 6.0,
            output: 30.0,
            cache_read: 0.6,
            cache_write: 7.5,
        }];
        let mut usage = Usage {
            input: 500,
            output: 1_000_000,
            ..Usage::default()
        };
        assert!((model.calculate_cost(&mut usage) - (0.0015 + 15.0)).abs() < 1e-9);

        let mut usage = Usage {
            input: 1_000_000,
            cache_read: 1_000_000,
            ..Usage::default()
        };
        model.calculate_cost(&mut usage);
        assert!((usage.cost.input - 6.0).abs() < 1e-9);
        assert!((usage.cost.cache_read - 0.6).abs() < 1e-9);
        assert!((usage.cost.total - 6.6).abs() < 1e-9);
    }

    #[test]
    fn thinking_level_parsing() {
        assert_eq!(
            "XHigh".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::XHigh
        );
        assert!("extreme".parse::<ThinkingLevel>().is_err());
    }
}
