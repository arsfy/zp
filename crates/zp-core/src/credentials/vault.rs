//! One protected keychain entry for all providers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, anyhow};

use super::SecretBackend;

const ACCOUNT: &str = "providers";
type Secrets = BTreeMap<String, String>;

enum State {
    Unloaded,
    Ready(Secrets),
    Failed(String),
}

pub(super) struct VaultBackend {
    vault: Arc<dyn SecretBackend>,
    state: Mutex<State>,
}

impl VaultBackend {
    pub(super) fn new(vault: Arc<dyn SecretBackend>) -> Self {
        Self {
            vault,
            state: Mutex::new(State::Unloaded),
        }
    }

    fn read(&self) -> anyhow::Result<Secrets> {
        if let Some(secret) = self.vault.get(ACCOUNT)? {
            return serde_json::from_str(&secret)
                .map_err(|_| anyhow!("the credential vault has an invalid format"));
        }
        Ok(Secrets::new())
    }

    fn write(&self, secrets: &Secrets) -> anyhow::Result<()> {
        self.vault
            .set(ACCOUNT, &serde_json::to_string(secrets)?)
            .context("could not save the credential vault")
    }

    fn load<'a>(&self, state: &'a mut State) -> anyhow::Result<&'a mut Secrets> {
        if matches!(state, State::Unloaded) {
            *state = match self.read() {
                Ok(secrets) => State::Ready(secrets),
                Err(error) => State::Failed(format!("{error:#}")),
            };
        }
        match state {
            State::Ready(secrets) => Ok(secrets),
            State::Failed(error) => Err(anyhow!("{error}")),
            State::Unloaded => unreachable!(),
        }
    }

    /// Applies `change` to the stored vault and writes it back. The vault is read again
    /// first: another process (the CLI next to the app) may have changed it since it was
    /// cached, and writing the cached copy would undo that.
    fn update(&self, change: impl FnOnce(&mut Secrets)) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut secrets = match self.read() {
            Ok(secrets) => secrets,
            Err(error) => {
                // A working cache stays usable; otherwise this read is the one that failed.
                if !matches!(*state, State::Ready(_)) {
                    *state = State::Failed(format!("{error:#}"));
                }
                return Err(error);
            }
        };
        change(&mut secrets);
        self.write(&secrets)?;
        *state = State::Ready(secrets);
        Ok(())
    }
}

impl SecretBackend for VaultBackend {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.load(&mut state)?.get(provider).cloned())
    }

    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
        self.update(|secrets| {
            secrets.insert(provider.to_owned(), secret.to_owned());
        })
    }

    fn delete(&self, provider: &str) -> anyhow::Result<()> {
        self.update(|secrets| {
            secrets.remove(provider);
        })
    }

    /// Reads the stored vault again, unless reading it failed before: such a read (a
    /// denied keychain prompt) is only retried by an edit or [`Self::retry_failed`]. A
    /// failure here keeps the cached vault.
    fn reload(&self, provider: &str) -> anyhow::Result<Option<String>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let State::Failed(error) = &*state {
            return Err(anyhow!("{error}"));
        }
        let secrets = self.read()?;
        let secret = secrets.get(provider).cloned();
        *state = State::Ready(secrets);
        Ok(secret)
    }

    fn retry_failed(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*state, State::Failed(_)) {
            *state = State::Unloaded;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::credentials::MemoryBackend;

    #[derive(Default)]
    struct Backend {
        inner: MemoryBackend,
        reads: AtomicUsize,
        writes: AtomicUsize,
        fail_read: AtomicBool,
        fail_write: AtomicBool,
    }

    impl SecretBackend for Backend {
        fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail_read.load(Ordering::SeqCst), "read denied");
            self.inner.get(provider)
        }

        fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail_write.load(Ordering::SeqCst), "write denied");
            self.inner.set(provider, secret)
        }

        fn delete(&self, provider: &str) -> anyhow::Result<()> {
            self.inner.delete(provider)
        }
    }

    #[test]
    fn restarts_read_one_vault_for_all_providers() {
        let storage = Arc::new(Backend::default());
        let vault = VaultBackend::new(storage.clone());
        vault.set("one", "sk-one").unwrap();
        let oauth =
            zp_ai::Credential::OAuth(zp_ai::OAuthCredential::new("access", "refresh", 60., 0));
        vault.set("two", &oauth.encode()).unwrap();
        assert_eq!(vault.get("one").unwrap().as_deref(), Some("sk-one"));
        assert_eq!(
            zp_ai::Credential::decode(&vault.get("two").unwrap().unwrap()),
            oauth
        );
        // Each save reads the stored vault first; reads come from the cache.
        assert_eq!(storage.reads.load(Ordering::SeqCst), 2);
        assert_eq!(storage.writes.load(Ordering::SeqCst), 2);

        let restarted = VaultBackend::new(storage.clone());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    assert_eq!(restarted.get("one").unwrap().as_deref(), Some("sk-one"));
                    assert_eq!(
                        zp_ai::Credential::decode(&restarted.get("two").unwrap().unwrap()),
                        oauth
                    );
                });
            }
        });
        assert_eq!(storage.reads.load(Ordering::SeqCst), 3);
        assert_eq!(storage.writes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn saves_keep_credentials_another_process_stored() {
        let storage = Arc::new(Backend::default());
        let app = VaultBackend::new(storage.clone());
        let cli = VaultBackend::new(storage.clone());
        app.set("one", "sk-one").unwrap();
        // The CLI loaded the vault before the app saved.
        assert_eq!(cli.get("one").unwrap().as_deref(), Some("sk-one"));
        app.set("one", "sk-one-new").unwrap();
        app.set("two", "sk-two").unwrap();
        cli.set("three", "sk-three").unwrap();
        app.delete("two").unwrap();

        let restarted = VaultBackend::new(storage);
        assert_eq!(restarted.get("one").unwrap().as_deref(), Some("sk-one-new"));
        assert_eq!(restarted.get("two").unwrap(), None);
        assert_eq!(restarted.get("three").unwrap().as_deref(), Some("sk-three"));
        assert_eq!(cli.get("one").unwrap().as_deref(), Some("sk-one-new"));
    }

    #[test]
    fn reload_reads_the_stored_vault_but_not_after_a_failed_read() {
        let storage = Arc::new(Backend::default());
        let app = VaultBackend::new(storage.clone());
        let cli = VaultBackend::new(storage.clone());
        app.set("one", "old").unwrap();
        assert_eq!(cli.get("one").unwrap().as_deref(), Some("old"));
        app.set("one", "new").unwrap();
        assert_eq!(cli.get("one").unwrap().as_deref(), Some("old"));
        assert_eq!(cli.reload("one").unwrap().as_deref(), Some("new"));
        assert_eq!(cli.get("one").unwrap().as_deref(), Some("new"));

        storage.fail_read.store(true, Ordering::SeqCst);
        let denied = VaultBackend::new(storage.clone());
        assert!(denied.get("one").is_err());
        let reads = storage.reads.load(Ordering::SeqCst);
        storage.fail_read.store(false, Ordering::SeqCst);
        assert!(denied.reload("one").is_err());
        assert!(denied.get("one").is_err());
        assert_eq!(storage.reads.load(Ordering::SeqCst), reads);
    }

    #[test]
    fn retry_failed_reads_the_vault_again() {
        let storage = Arc::new(Backend::default());
        VaultBackend::new(storage.clone())
            .set("one", "sk-one")
            .unwrap();
        storage.fail_read.store(true, Ordering::SeqCst);
        let vault = VaultBackend::new(storage.clone());
        assert!(vault.get("one").is_err());
        storage.fail_read.store(false, Ordering::SeqCst);
        assert!(vault.get("one").is_err());
        vault.retry_failed();
        assert_eq!(vault.get("one").unwrap().as_deref(), Some("sk-one"));
        // A loaded vault is kept.
        let reads = storage.reads.load(Ordering::SeqCst);
        vault.retry_failed();
        assert_eq!(vault.get("one").unwrap().as_deref(), Some("sk-one"));
        assert_eq!(storage.reads.load(Ordering::SeqCst), reads);
    }

    #[test]
    fn failed_reads_are_not_retried_until_an_explicit_save() {
        let storage = Arc::new(Backend::default());
        storage.fail_read.store(true, Ordering::SeqCst);
        let vault = VaultBackend::new(storage.clone());
        assert!(vault.get("one").is_err());
        assert!(vault.get("two").is_err());
        assert_eq!(storage.reads.load(Ordering::SeqCst), 1);
        assert_eq!(storage.writes.load(Ordering::SeqCst), 0);
        storage.fail_read.store(false, Ordering::SeqCst);
        vault.set("one", "new-key").unwrap();
        assert_eq!(vault.get("one").unwrap().as_deref(), Some("new-key"));
    }

    #[test]
    fn missing_vault_is_empty_and_reads_do_not_create_it() {
        let storage = Arc::new(Backend::default());
        let vault = VaultBackend::new(storage.clone());
        assert_eq!(vault.get("one").unwrap(), None);
        assert_eq!(vault.get("two").unwrap(), None);
        assert_eq!(storage.reads.load(Ordering::SeqCst), 1);
        assert_eq!(storage.writes.load(Ordering::SeqCst), 0);
        assert_eq!(storage.inner.get(ACCOUNT).unwrap(), None);
    }

    #[test]
    fn failed_writes_keep_cached_and_persisted_credentials() {
        let storage = Arc::new(Backend::default());
        let vault = VaultBackend::new(storage.clone());
        vault.set("one", "original").unwrap();
        storage.fail_write.store(true, Ordering::SeqCst);
        assert!(vault.set("one", "replacement").is_err());
        assert!(vault.delete("one").is_err());
        assert_eq!(vault.get("one").unwrap().as_deref(), Some("original"));
        let restarted = VaultBackend::new(storage);
        assert_eq!(restarted.get("one").unwrap().as_deref(), Some("original"));
    }

    #[test]
    fn removal_persists_and_preserves_other_providers() {
        let storage = Arc::new(Backend::default());
        let vault = VaultBackend::new(storage.clone());
        vault.set("one", "sk-one").unwrap();
        vault.set("two", "sk-two").unwrap();
        vault.delete("one").unwrap();
        assert!(vault.get("one").unwrap().is_none());
        let restarted = VaultBackend::new(storage);
        assert!(restarted.get("one").unwrap().is_none());
        assert_eq!(restarted.get("two").unwrap().as_deref(), Some("sk-two"));
    }

    #[test]
    fn invalid_vault_is_not_overwritten_or_included_in_errors() {
        let storage = Arc::new(Backend::default());
        storage.set(ACCOUNT, "\"secret-invalid-vault\"").unwrap();
        let vault = VaultBackend::new(storage.clone());
        let error = vault.get("one").unwrap_err().to_string();
        assert!(!error.contains("secret-invalid-vault"));
        assert!(vault.set("one", "new-key").is_err());
        assert_eq!(storage.writes.load(Ordering::SeqCst), 1);
    }
}
