//! Per-request options.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::model::ThinkingLevel;

/// Token budgets for providers that size reasoning in tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinkingBudgets {
    /// Budget for `minimal`.
    pub minimal: u64,
    /// Budget for `low`.
    pub low: u64,
    /// Budget for `medium`.
    pub medium: u64,
    /// Budget for `high` (and `xhigh` / `max`).
    pub high: u64,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self {
            minimal: 1024,
            low: 2048,
            medium: 8192,
            high: 16384,
        }
    }
}

impl ThinkingBudgets {
    /// Budget for `level` (`0` for `Off`).
    pub fn for_level(&self, level: ThinkingLevel) -> u64 {
        match level {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => self.minimal,
            ThinkingLevel::Low => self.low,
            ThinkingLevel::Medium => self.medium,
            ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => self.high,
        }
    }
}

/// Default number of retries for rate limits and transient server errors.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Preferred prompt-cache lifetime. Adapters send controls supported by the model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    /// Skip client-directed cache writes and cache keys.
    None,
    /// Use the provider's normal cache lifetime.
    #[default]
    Short,
    /// Request an extended lifetime on supported models.
    Long,
}

impl CacheRetention {
    pub(crate) fn anthropic_control(self, supports_long: bool) -> Option<serde_json::Value> {
        match self {
            Self::None => None,
            Self::Long if supports_long => {
                Some(serde_json::json!({"type": "ephemeral", "ttl": "1h"}))
            }
            _ => Some(serde_json::json!({"type": "ephemeral"})),
        }
    }
}

/// Options for one streaming request.
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    /// API key. Filled in by [`crate::Models`] when not set explicitly.
    pub api_key: Option<String>,
    /// Requested reasoning level; clamped to what the model supports.
    pub thinking: ThinkingLevel,
    /// Overrides for token-based reasoning budgets.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Output token limit (defaults to the model maximum, fitted into the context window).
    pub max_tokens: Option<u64>,
    /// Sampling temperature, where supported.
    pub temperature: Option<f32>,
    /// Extra request headers; these override model and provider headers.
    pub headers: HashMap<String, String>,
    /// Stable session id used for provider-side caching hints.
    pub session_id: Option<String>,
    /// Prompt-cache policy for adapters that support explicit controls.
    pub cache_retention: CacheRetention,
    /// Cancels the request; the stream then ends with an `Aborted` error.
    pub cancel: CancellationToken,
    /// Retries before the stream starts (defaults to [`DEFAULT_MAX_RETRIES`]).
    pub max_retries: Option<u32>,
}
