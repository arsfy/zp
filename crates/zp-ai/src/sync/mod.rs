//! Keeping model catalogs current.
//!
//! The compiled-in catalog (see [`crate::builtin_providers`]) is the baseline. Two layers
//! can be applied on top of it at runtime through [`crate::Models`]:
//!
//! 1. [`ModelsDevCatalog`] — metadata from <https://models.dev>: refreshes limits, pricing
//!    and modalities of known models and adds new tool-capable chat models.
//! 2. [`UpstreamModels`] — the model list a configured provider reports for the current
//!    credential (`GET /v1/models` or `GET /models`). It decides availability: known models
//!    the provider no longer lists are hidden and unknown listed models are added.
//!
//! Fetching and applying are separate so callers decide when to hit the network and how
//! to persist results (`zp-core` caches them under `~/.zp/cache/models/`).

mod merge;
mod models_dev;
mod upstream;

use std::fmt;

pub(crate) use merge::resolve_models;
pub use models_dev::{
    MODELS_DEV_URL, ModelsDevCatalog, ModelsDevCost, ModelsDevCostTier, ModelsDevFetch,
    ModelsDevLimits, ModelsDevModalities, ModelsDevModel, ModelsDevProvider, ModelsDevRates,
    ModelsDevReasoningOption, ModelsDevTierSpec, fetch_models_dev,
};
pub use upstream::{UpstreamModel, UpstreamModels, list_upstream_models};

/// Why a catalog could not be refreshed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    /// The provider cannot list its models.
    Unsupported,
    /// The provider needs credentials and none are configured.
    NoCredentials,
    /// Cancelled through the caller's token.
    Cancelled,
    /// A network, HTTP or parsing failure.
    Failed(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("model listing is not supported"),
            Self::NoCredentials => f.write_str("no credentials configured"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<crate::http::HttpError> for SyncError {
    fn from(error: crate::http::HttpError) -> Self {
        match error {
            crate::http::HttpError::Cancelled => Self::Cancelled,
            crate::http::HttpError::Failed(message) => Self::Failed(message),
        }
    }
}
