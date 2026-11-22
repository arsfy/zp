//! Credentials and their resolution.
//!
//! A provider is authenticated by a stored [`Credential`] — an API key or an OAuth token
//! obtained by signing in (see [`crate::oauth`]) — or by an API key in one of its
//! environment variables. Stored credentials win over the environment.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::now_ms;

/// Tokens obtained by signing in to a provider.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    /// Access token sent with requests.
    pub access: String,
    /// Refresh token (empty when the provider issues long-lived tokens).
    #[serde(default)]
    pub refresh: String,
    /// When `access` expires, in milliseconds since the epoch (already reduced by a safety
    /// margin).
    pub expires: i64,
    /// Provider-specific data (account id, enterprise domain, issued client id…).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl OAuthCredential {
    /// A credential expiring `expires_in` seconds from now, minus `margin_ms`. The lifetime
    /// comes from the server, so it is kept between 0 and a year: a huge value must not
    /// overflow (and wrap into "always expired").
    pub fn new(
        access: impl Into<String>,
        refresh: impl Into<String>,
        expires_in: f64,
        margin_ms: i64,
    ) -> Self {
        const MAX_LIFETIME_SECONDS: f64 = 365.0 * 24.0 * 3600.0;
        let lifetime_ms = if expires_in.is_nan() {
            0
        } else {
            // Float to integer `as` saturates; the clamp keeps it meaningful.
            (expires_in.clamp(0.0, MAX_LIFETIME_SECONDS) * 1000.0) as i64
        };
        Self::expiring_at(
            access,
            refresh,
            now_ms().saturating_add(lifetime_ms),
            margin_ms,
        )
    }

    /// A credential expiring at `expires_ms` (Unix milliseconds), minus `margin_ms`.
    pub fn expiring_at(
        access: impl Into<String>,
        refresh: impl Into<String>,
        expires_ms: i64,
        margin_ms: i64,
    ) -> Self {
        Self {
            access: access.into(),
            refresh: refresh.into(),
            expires: expires_ms.saturating_sub(margin_ms),
            extra: Map::new(),
        }
    }

    /// Whether the access token has expired.
    pub fn is_expired(&self) -> bool {
        now_ms() >= self.expires
    }

    /// A string from the provider-specific data.
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Sets provider-specific data.
    pub fn with_extra(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_owned(), value.into());
        self
    }
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("expires", &self.expires)
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A stored credential.
#[derive(Clone, PartialEq)]
pub enum Credential {
    /// An API key.
    ApiKey(String),
    /// Tokens from signing in.
    OAuth(OAuthCredential),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Encoded {
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    /// The text kept in a secret store: an API key as is, OAuth tokens as JSON.
    pub fn encode(&self) -> String {
        match self {
            Self::ApiKey(key) => key.clone(),
            Self::OAuth(credential) => {
                serde_json::to_string(&Encoded::OAuth(credential.clone())).unwrap_or_default()
            }
        }
    }

    /// Reads [`Self::encode`]'s output (plain text is an API key).
    pub fn decode(text: &str) -> Self {
        if text.trim_start().starts_with('{')
            && let Ok(Encoded::OAuth(credential)) = serde_json::from_str(text)
        {
            return Self::OAuth(credential);
        }
        Self::ApiKey(text.to_owned())
    }

    /// Whether this is an OAuth credential.
    pub fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::OAuth(credential) => credential.fmt(f),
        }
    }
}

/// Source of stored credentials (e.g. the system keychain).
pub trait CredentialStore: Send + Sync {
    /// The stored credential of `provider`, if any.
    fn credential(&self, provider: &str) -> Option<Credential>;

    /// Stores `credential` for `provider` (refreshed OAuth tokens are written back).
    fn store(&self, provider: &str, credential: &Credential) -> Result<(), String> {
        let _ = (provider, credential);
        Err("this credential store is read-only".into())
    }

    /// The stored credential of `provider` read again, bypassing caches: another process
    /// may have changed it (refreshed OAuth tokens, for one).
    fn reload(&self, provider: &str) -> Option<Credential> {
        self.credential(provider)
    }

    /// The stored API key of `provider` (not OAuth tokens).
    fn api_key(&self, provider: &str) -> Option<String> {
        match self.credential(provider)? {
            Credential::ApiKey(key) => Some(key),
            Credential::OAuth(_) => None,
        }
    }
}

/// In-memory credential store.
#[derive(Default)]
pub struct MemoryCredentialStore {
    credentials: RwLock<HashMap<String, Credential>>,
}

impl MemoryCredentialStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores API key `key` for `provider`.
    pub fn set(&self, provider: impl Into<String>, key: impl Into<String>) {
        self.set_credential(provider, Credential::ApiKey(key.into()));
    }

    /// Stores `credential` for `provider`.
    pub fn set_credential(&self, provider: impl Into<String>, credential: Credential) {
        if let Ok(mut credentials) = self.credentials.write() {
            credentials.insert(provider.into(), credential);
        }
    }

    /// Removes the credential of `provider`.
    pub fn remove(&self, provider: &str) {
        if let Ok(mut credentials) = self.credentials.write() {
            credentials.remove(provider);
        }
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn credential(&self, provider: &str) -> Option<Credential> {
        self.credentials.read().ok()?.get(provider).cloned()
    }

    fn store(&self, provider: &str, credential: &Credential) -> Result<(), String> {
        self.set_credential(provider, credential.clone());
        Ok(())
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

/// Where a resolved credential came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSource {
    /// Passed explicitly with the request.
    Explicit,
    /// An API key from the credential store.
    Stored,
    /// Tokens from signing in, in the credential store.
    OAuth,
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
            Self::OAuth => f.write_str("signed in"),
            Self::Env(name) => f.write_str(name),
            Self::NotRequired => f.write_str("no key required"),
        }
    }
}

/// A resolved credential.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedAuth {
    /// The key (or current access token), absent for providers that need none.
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

/// What a request is authenticated with.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RequestAuth {
    /// Sent the protocol's usual way (`x-api-key` or `Authorization: Bearer`).
    pub api_key: Option<String>,
    /// Extra headers (they override model headers).
    pub headers: HashMap<String, String>,
    /// Replaces the model's base URL (e.g. a per-account endpoint).
    pub base_url: Option<String>,
}

impl fmt::Debug for RequestAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl RequestAuth {
    /// Authenticates with `key` only.
    pub fn api_key(key: impl Into<String>) -> Self {
        Self {
            api_key: Some(key.into()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lifetimes come from token endpoints: absurd values must not overflow into a credential
    /// that is always expired (refreshing before every request) or panic in debug builds.
    #[test]
    fn absurd_lifetimes_neither_overflow_nor_expire_at_once() {
        let year_ms = 365 * 24 * 3600 * 1000;
        let huge = OAuthCredential::new("a", "r", 1e300, 5 * 60 * 1000);
        assert!(!huge.is_expired());
        assert!(huge.expires <= now_ms() + year_ms);
        let infinite = OAuthCredential::new("a", "r", f64::INFINITY, 0);
        assert!(!infinite.is_expired());
        assert!(OAuthCredential::new("a", "r", -5.0, 0).is_expired());
        assert!(OAuthCredential::new("a", "r", f64::NAN, 0).is_expired());
        // Absolute times (Copilot's `expires_at`) at either extreme.
        assert_eq!(
            OAuthCredential::expiring_at("a", "r", i64::MIN, 1000).expires,
            i64::MIN
        );
        assert!(!OAuthCredential::expiring_at("a", "r", i64::MAX, 1000).is_expired());
    }

    #[test]
    fn credentials_round_trip_through_their_encoding() {
        let key = Credential::ApiKey("sk-test".into());
        assert_eq!(key.encode(), "sk-test");
        assert_eq!(Credential::decode("sk-test"), key);

        let oauth = Credential::OAuth(
            OAuthCredential::new("access", "refresh", 3600.0, 0).with_extra("accountId", "acc"),
        );
        let text = oauth.encode();
        assert!(text.contains("\"type\":\"oauth\""), "{text}");
        let decoded = Credential::decode(&text);
        assert_eq!(decoded, oauth);
        let Credential::OAuth(decoded) = decoded else {
            unreachable!()
        };
        assert_eq!(decoded.extra_str("accountId"), Some("acc"));
        assert!(!decoded.is_expired());

        // A key that happens to look like JSON stays a key.
        assert!(!Credential::decode("{not json").is_oauth());
    }
}
