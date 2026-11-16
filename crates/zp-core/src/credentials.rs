//! Credentials in the system keychain (macOS Keychain, Windows Credential Manager, Secret
//! Service on Linux): API keys as they are, and tokens from signing in to a provider as
//! JSON (see [`zp_ai::Credential::encode`]).
//!
//! The keychain is only queried for providers listed in a small index file
//! (`~/.zp/credentials.json`, ids only — never secrets), so providers without a stored key
//! cost nothing and trigger no keychain prompts. Keys are cached in memory once read.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use anyhow::Context as _;
use zp_ai::{Credential, CredentialStore};

/// Keychain service name of ZP's API keys.
pub const KEYCHAIN_SERVICE: &str = "zp.api-keys";

/// Where secrets live.
pub trait SecretBackend: Send + Sync {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()>;
    fn delete(&self, provider: &str) -> anyhow::Result<()>;
}

/// The platform keychain.
pub struct KeyringBackend {
    service: String,
}

impl KeyringBackend {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, provider: &str) -> anyhow::Result<keyring::Entry> {
        keyring::Entry::new(&self.service, provider).context("could not open the keychain")
    }
}

impl SecretBackend for KeyringBackend {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        match self.entry(provider)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("could not read the keychain"),
        }
    }

    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
        self.entry(provider)?
            .set_password(secret)
            .context("could not write to the keychain")
    }

    fn delete(&self, provider: &str) -> anyhow::Result<()> {
        match self.entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("could not delete from the keychain"),
        }
    }
}

/// Secrets in memory (tests, or no keychain).
#[derive(Default)]
pub struct MemoryBackend {
    secrets: Mutex<HashMap<String, String>>,
}

impl SecretBackend for MemoryBackend {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(provider)
            .cloned())
    }

    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
        self.secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(provider.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, provider: &str) -> anyhow::Result<()> {
        self.secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(provider);
        Ok(())
    }
}

#[derive(Default)]
struct State {
    /// Providers with a stored key.
    stored: BTreeSet<String>,
    /// Keys read so far.
    cache: HashMap<String, String>,
}

/// Stored API keys: a [`CredentialStore`] for [`zp_ai::Models`] that can also change them.
pub struct Credentials {
    backend: Arc<dyn SecretBackend>,
    index: Option<PathBuf>,
    state: RwLock<State>,
}

impl Credentials {
    /// Keys in `backend`, with the index of stored providers at `index` (in memory without
    /// one).
    pub fn new(backend: Arc<dyn SecretBackend>, index: Option<PathBuf>) -> Self {
        let stored = index
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<BTreeSet<String>>(&text).ok())
            .unwrap_or_default();
        Self {
            backend,
            index,
            state: RwLock::new(State {
                stored,
                cache: HashMap::new(),
            }),
        }
    }

    /// The system keychain, indexed in `data_dir`.
    pub fn keychain(data_dir: &Path) -> Self {
        Self::new(
            Arc::new(KeyringBackend::new(KEYCHAIN_SERVICE)),
            Some(data_dir.join("credentials.json")),
        )
    }

    /// Keys kept in memory only.
    pub fn in_memory() -> Self {
        Self::new(Arc::new(MemoryBackend::default()), None)
    }

    /// Stores API key `key` for `provider` (replacing a sign-in).
    pub fn set(&self, provider: &str, key: &str) -> anyhow::Result<()> {
        let key = key.trim();
        anyhow::ensure!(!key.is_empty(), "the key is empty");
        self.set_secret(provider, key)
    }

    /// Stores `credential` for `provider`.
    pub fn set_credential(&self, provider: &str, credential: &Credential) -> anyhow::Result<()> {
        if let Credential::ApiKey(key) = credential {
            return self.set(provider, key);
        }
        self.set_secret(provider, &credential.encode())
    }

    fn set_secret(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
        self.backend.set(provider, secret)?;
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        state.cache.insert(provider.to_owned(), secret.to_owned());
        state.stored.insert(provider.to_owned());
        self.save_index(&state.stored)
    }

    /// Deletes the stored key or sign-in of `provider`.
    pub fn remove(&self, provider: &str) -> anyhow::Result<()> {
        self.backend.delete(provider)?;
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        state.cache.remove(provider);
        state.stored.remove(provider);
        self.save_index(&state.stored)
    }

    /// Whether a key is stored for `provider`.
    pub fn is_stored(&self, provider: &str) -> bool {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .stored
            .contains(provider)
    }

    fn save_index(&self, stored: &BTreeSet<String>) -> anyhow::Result<()> {
        let Some(path) = &self.index else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(stored)?)
            .with_context(|| format!("could not write {}", path.display()))
    }
}

impl CredentialStore for Credentials {
    fn credential(&self, provider: &str) -> Option<Credential> {
        self.secret(provider)
            .map(|secret| Credential::decode(&secret))
    }

    fn store(&self, provider: &str, credential: &Credential) -> Result<(), String> {
        self.set_credential(provider, credential)
            .map_err(|error| format!("{error:#}"))
    }
}

impl Credentials {
    /// The stored secret of `provider`, read from the keychain once.
    fn secret(&self, provider: &str) -> Option<String> {
        {
            let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
            if !state.stored.contains(provider) {
                return None;
            }
            if let Some(key) = state.cache.get(provider) {
                return Some(key.clone());
            }
        }
        match self.backend.get(provider) {
            Ok(Some(key)) => {
                self.state
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .cache
                    .insert(provider.to_owned(), key.clone());
                Some(key)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(provider, error = %format!("{error:#}"), "could not read a credential");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts reads to check the index and cache.
    #[derive(Default)]
    struct Counting {
        inner: MemoryBackend,
        reads: Mutex<usize>,
    }

    impl SecretBackend for Counting {
        fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
            *self.reads.lock().unwrap() += 1;
            self.inner.get(provider)
        }
        fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
            self.inner.set(provider, secret)
        }
        fn delete(&self, provider: &str) -> anyhow::Result<()> {
            self.inner.delete(provider)
        }
    }

    #[test]
    fn index_cache_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("credentials.json");
        let backend = Arc::new(Counting::default());
        let credentials = Credentials::new(backend.clone(), Some(index.clone()));
        assert_eq!(credentials.api_key("openai"), None);
        assert_eq!(
            *backend.reads.lock().unwrap(),
            0,
            "not stored: no keychain access"
        );
        assert!(credentials.set("openai", "  ").is_err());
        credentials.set("openai", " sk-test \n").unwrap();
        assert_eq!(credentials.api_key("openai").as_deref(), Some("sk-test"));
        assert!(!std::fs::read_to_string(&index).unwrap().contains("sk-test"));

        // A new process reads the keychain once, then the cache.
        let again = Credentials::new(backend.clone(), Some(index.clone()));
        assert!(again.is_stored("openai"));
        assert_eq!(again.api_key("openai").as_deref(), Some("sk-test"));
        assert_eq!(again.api_key("openai").as_deref(), Some("sk-test"));
        assert_eq!(*backend.reads.lock().unwrap(), 1);

        again.remove("openai").unwrap();
        assert_eq!(again.api_key("openai"), None);
        assert!(!Credentials::new(backend, Some(index)).is_stored("openai"));
    }

    #[test]
    fn models_resolve_stored_keys() {
        let credentials = Arc::new(Credentials::in_memory());
        let models = zp_ai::Models::builtin()
            .with_env(Arc::new(|_| None))
            .with_credentials(credentials.clone());
        assert!(models.resolve_auth("deepseek").is_none());
        credentials.set("deepseek", "sk-1").unwrap();
        let auth = models.resolve_auth("deepseek").unwrap();
        assert_eq!(auth.source, zp_ai::AuthSource::Stored);
    }
}
