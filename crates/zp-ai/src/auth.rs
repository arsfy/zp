//! API key resolution.
//!
//! Order: an explicit key on the request, then the credential store, then the provider's
//! environment variables.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

/// Source of stored API keys (e.g. the system keychain, added in a later milestone).
pub trait CredentialStore: Send + Sync {
    /// Returns the stored key for `provider`, if any.
    fn api_key(&self, provider: &str) -> Option<String>;
}

/// In-memory credential store.
#[derive(Default)]
pub struct MemoryCredentialStore {
    keys: RwLock<HashMap<String, String>>,
}

impl MemoryCredentialStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `key` for `provider`.
    pub fn set(&self, provider: impl Into<String>, key: impl Into<String>) {
        if let Ok(mut keys) = self.keys.write() {
            keys.insert(provider.into(), key.into());
        }
    }

    /// Removes the key for `provider`.
    pub fn remove(&self, provider: &str) {
        if let Ok(mut keys) = self.keys.write() {
            keys.remove(provider);
        }
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn api_key(&self, provider: &str) -> Option<String> {
        self.keys.read().ok()?.get(provider).cloned()
    }
}

/// Reads environment variables; replaceable for tests.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, ignoring empty values.
pub fn process_env() -> EnvLookup {
    Arc::new(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

/// Where a resolved key came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSource {
    /// Passed explicitly with the request.
    Explicit,
    /// Read from the credential store.
    Stored,
    /// Read from the named environment variable.
    Env(String),
    /// The provider does not need a key.
    NotRequired,
}

impl fmt::Display for AuthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Explicit => f.write_str("explicit key"),
            Self::Stored => f.write_str("stored credential"),
            Self::Env(name) => f.write_str(name),
            Self::NotRequired => f.write_str("no key required"),
        }
    }
}

/// A resolved credential.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedAuth {
    /// The key, absent for providers that need none.
    pub api_key: Option<String>,
    /// Where it came from.
    pub source: AuthSource,
}

impl fmt::Debug for ResolvedAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("source", &self.source)
            .finish()
    }
}
