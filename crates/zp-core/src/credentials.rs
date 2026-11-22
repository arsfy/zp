//! Credentials in the system keychain (macOS Keychain, Windows Credential Manager, Secret
//! Service on Linux): API keys as they are, and tokens from signing in to a provider as
//! JSON (see [`zp_ai::Credential::encode`]).
//!
//! Credentials share one keychain entry, unlocked once per process.
//! The keychain is only queried for providers listed in a small index file
//! (`~/.zp/credentials.json`, ids only — never secrets), so providers without a stored key
//! cost nothing and trigger no keychain prompts. Keys are cached in memory once read.
//!
//! The app and the CLI may run side by side: saves start from the stored vault and index,
//! not this process's copy, so they keep what the other process stored.

mod vault;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use anyhow::Context as _;
use zp_ai::{Credential, CredentialStore};

/// Keychain service name of ZP's credential vault.
pub const KEYCHAIN_SERVICE: &str = "zp.credentials";

/// Where secrets live.
pub trait SecretBackend: Send + Sync {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>>;
    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()>;
    fn delete(&self, provider: &str) -> anyhow::Result<()>;

    /// Reads `provider` again, bypassing any cache (another process may have changed it).
    fn reload(&self, provider: &str) -> anyhow::Result<Option<String>> {
        self.get(provider)
    }

    /// Lets the next read retry a read that failed (and is cached as failed).
    fn retry_failed(&self) {}
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
    /// Results read so far, including missing or inaccessible keys.
    cache: HashMap<String, Option<String>>,
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
        let stored = index.as_deref().and_then(read_index).unwrap_or_default();
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
        let keyring = Arc::new(KeyringBackend::new(KEYCHAIN_SERVICE));
        Self::new(
            Arc::new(vault::VaultBackend::new(keyring)),
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
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        self.backend.set(provider, secret)?;
        state.cache.retain(|_, secret| secret.is_some());
        state
            .cache
            .insert(provider.to_owned(), Some(secret.to_owned()));
        state.stored.insert(provider.to_owned());
        self.save_index(&mut state.stored, provider, true)
    }

    /// Deletes the stored key or sign-in of `provider`.
    pub fn remove(&self, provider: &str) -> anyhow::Result<()> {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        self.backend.delete(provider)?;
        state.cache.retain(|_, secret| secret.is_some());
        state.cache.remove(provider);
        state.stored.remove(provider);
        self.save_index(&mut state.stored, provider, false)
    }

    /// Lets the next reads retry keys that could not be read (or were missing), e.g. after
    /// a denied keychain prompt. Failed reads are otherwise not retried, so a denied prompt
    /// does not come back on every request; call this on an explicit user action only.
    pub fn retry_failed(&self) {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        state.cache.retain(|_, secret| secret.is_some());
        self.backend.retry_failed();
    }

    /// Whether a key is stored for `provider`.
    pub fn is_stored(&self, provider: &str) -> bool {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .stored
            .contains(provider)
    }

    /// Records in the index whether `provider` has a key (`present`), then makes `stored`
    /// the saved index. The saved index is read again first so that providers another
    /// process added or removed meanwhile stay that way; `stored` is only the fallback when
    /// it cannot be read.
    fn save_index(
        &self,
        stored: &mut BTreeSet<String>,
        provider: &str,
        present: bool,
    ) -> anyhow::Result<()> {
        let Some(path) = &self.index else {
            return Ok(());
        };
        let mut index = read_index(path).unwrap_or_else(|| stored.clone());
        if present {
            index.insert(provider.to_owned());
        } else {
            index.remove(provider);
        }
        crate::fs::write_atomic(path, serde_json::to_string_pretty(&index)?.as_bytes())?;
        *stored = index;
        Ok(())
    }
}

/// The providers listed in the index at `path` (none when it does not exist), or `None`
/// when it cannot be read. An invalid index is moved aside so that saving does not
/// destroy the list of keys it held.
fn read_index(path: &Path) -> Option<BTreeSet<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(BTreeSet::new());
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not read the credential index");
            return None;
        }
    };
    serde_json::from_str(&text)
        .inspect_err(|error| {
            crate::fs::move_aside_corrupt(path, error);
        })
        .ok()
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

    /// Reads the stored credential from the keychain again, replacing the cached one. A
    /// read that failed before is not retried (see [`Credentials::retry_failed`]).
    fn reload(&self, provider: &str) -> Option<Credential> {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if !state.stored.contains(provider) || matches!(state.cache.get(provider), Some(None)) {
            return None;
        }
        match self.backend.reload(provider) {
            Ok(Some(secret)) => {
                let credential = Credential::decode(&secret);
                state.cache.insert(provider.to_owned(), Some(secret));
                Some(credential)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(provider, error = %format!("{error:#}"), "could not reload a credential");
                None
            }
        }
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
            if let Some(secret) = state.cache.get(provider) {
                return secret.clone();
            }
        }
        // Serialize cache misses so startup tasks cannot unlock the same entry twice.
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if !state.stored.contains(provider) {
            return None;
        }
        if let Some(secret) = state.cache.get(provider) {
            return secret.clone();
        }
        let secret = match self.backend.get(provider) {
            Ok(secret) => secret,
            Err(error) => {
                tracing::warn!(provider, error = %format!("{error:#}"), "could not read a credential");
                None
            }
        };
        state.cache.insert(provider.to_owned(), secret.clone());
        secret
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Counts reads to check the index and cache.
    #[derive(Default)]
    struct Counting {
        inner: MemoryBackend,
        reads: Mutex<usize>,
        fail_read: AtomicBool,
    }

    impl SecretBackend for Counting {
        fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
            *self.reads.lock().unwrap() += 1;
            anyhow::ensure!(!self.fail_read.load(Ordering::SeqCst), "read denied");
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

    #[test]
    fn concurrent_cache_misses_read_the_backend_once() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("credentials.json");
        let backend = Arc::new(Counting::default());
        Credentials::new(backend.clone(), Some(index.clone()))
            .set("one", "sk-one")
            .unwrap();
        let credentials = Credentials::new(backend.clone(), Some(index));
        let barrier = Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    assert_eq!(credentials.api_key("one").as_deref(), Some("sk-one"));
                });
            }
        });
        assert_eq!(*backend.reads.lock().unwrap(), 1);
    }

    #[test]
    fn missing_credentials_are_cached() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("credentials.json");
        let backend = Arc::new(Counting::default());
        std::fs::write(&index, r#"["missing"]"#).unwrap();
        let credentials = Credentials::new(backend.clone(), Some(index));
        assert_eq!(credentials.api_key("missing"), None);
        assert_eq!(credentials.api_key("missing"), None);
        assert_eq!(*backend.reads.lock().unwrap(), 1);
    }

    #[test]
    fn denied_reads_are_cached_until_a_successful_edit() {
        for remove in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let index = dir.path().join("credentials.json");
            let backend = Arc::new(Counting::default());
            let credentials = Credentials::new(backend.clone(), Some(index.clone()));
            credentials.set("one", "sk-one").unwrap();
            credentials.set("two", "sk-two").unwrap();
            let credentials = Credentials::new(backend.clone(), Some(index));
            backend.fail_read.store(true, Ordering::SeqCst);
            assert_eq!(credentials.api_key("one"), None);
            assert_eq!(credentials.api_key("one"), None);
            assert_eq!(credentials.api_key("two"), None);
            assert_eq!(*backend.reads.lock().unwrap(), 2);

            backend.fail_read.store(false, Ordering::SeqCst);
            if remove {
                credentials.remove("two").unwrap();
            } else {
                credentials.set("two", "replacement").unwrap();
            }
            assert_eq!(credentials.api_key("one").as_deref(), Some("sk-one"));
            assert_eq!(*backend.reads.lock().unwrap(), 3);
            assert_eq!(
                credentials.api_key("two").as_deref(),
                if remove { None } else { Some("replacement") }
            );
        }
    }

    /// One process's credentials (the app or the CLI): its own vault cache over the shared
    /// `keychain`, and the index in `dir`.
    fn process(keychain: &Arc<MemoryBackend>, dir: &Path) -> Credentials {
        Credentials::new(
            Arc::new(vault::VaultBackend::new(keychain.clone())),
            Some(dir.join("credentials.json")),
        )
    }

    #[test]
    fn saves_keep_what_another_process_stored() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Arc::new(MemoryBackend::default());
        let app = process(&keychain, dir.path());
        let cli = process(&keychain, dir.path());
        app.set("one", "sk-one").unwrap();
        cli.set("two", "sk-two").unwrap();
        app.set("three", "sk-three").unwrap();
        cli.remove("three").unwrap();
        app.set("four", "sk-four").unwrap();
        // A save picks up the other process's index too.
        assert!(app.is_stored("two"));
        assert!(!app.is_stored("three"));

        let restarted = process(&keychain, dir.path());
        assert_eq!(restarted.api_key("one").as_deref(), Some("sk-one"));
        assert_eq!(restarted.api_key("two").as_deref(), Some("sk-two"));
        assert!(!restarted.is_stored("three"));
        assert_eq!(restarted.api_key("four").as_deref(), Some("sk-four"));
    }

    #[test]
    fn reload_reads_tokens_another_process_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Arc::new(MemoryBackend::default());
        let app = process(&keychain, dir.path());
        let old = Credential::OAuth(zp_ai::OAuthCredential::new("old", "refresh-1", 60., 0));
        let new = Credential::OAuth(zp_ai::OAuthCredential::new("new", "refresh-2", 60., 0));
        app.set_credential("codex", &old).unwrap();
        let cli = process(&keychain, dir.path());
        assert_eq!(cli.credential("codex"), Some(old));
        app.set_credential("codex", &new).unwrap();
        assert_eq!(cli.reload("codex"), Some(new.clone()));
        assert_eq!(cli.credential("codex"), Some(new));
        assert_eq!(cli.reload("missing"), None);
    }

    #[test]
    fn retry_failed_reads_denied_keys_again_and_reload_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("credentials.json");
        let backend = Arc::new(Counting::default());
        Credentials::new(backend.clone(), Some(index.clone()))
            .set("one", "sk-one")
            .unwrap();
        let credentials = Credentials::new(backend.clone(), Some(index));
        backend.fail_read.store(true, Ordering::SeqCst);
        assert_eq!(credentials.api_key("one"), None);
        backend.fail_read.store(false, Ordering::SeqCst);
        assert_eq!(credentials.reload("one"), None);
        assert_eq!(credentials.api_key("one"), None);
        assert_eq!(*backend.reads.lock().unwrap(), 1);

        credentials.retry_failed();
        assert_eq!(credentials.api_key("one").as_deref(), Some("sk-one"));
        assert_eq!(*backend.reads.lock().unwrap(), 2);
    }

    #[test]
    fn an_invalid_index_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("credentials.json");
        std::fs::write(&index, "[\"one\", truncated").unwrap();
        let credentials = Credentials::new(Arc::new(MemoryBackend::default()), Some(index.clone()));
        assert!(!credentials.is_stored("one"));
        credentials.set("two", "sk-two").unwrap();

        let backups: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("credentials.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&backups[0])).unwrap(),
            "[\"one\", truncated"
        );
        let saved: BTreeSet<String> =
            serde_json::from_str(&std::fs::read_to_string(&index).unwrap()).unwrap();
        assert_eq!(saved, BTreeSet::from(["two".to_owned()]));
    }
}
