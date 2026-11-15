//! Persisted model catalog layers and their refresh.
//!
//! [`zp_ai::Models`] starts from the compiled-in catalog. This module stores the two runtime
//! layers under `~/.zp/cache/models/` so they survive restarts:
//!
//! ```text
//! cache/models/models.dev.json          trimmed models.dev catalog + ETag
//! cache/models/upstream/<provider>.json last model list reported by the provider
//! ```
//!
//! Startup calls [`ModelCatalogCache::load`] (synchronous, no network). A background task
//! (or `zp-cli sync`) calls [`ModelCatalogCache::sync`], which refreshes stale layers,
//! applies them to the shared `Models` and writes them back.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use zp_ai::{
    CancellationToken, MODELS_DEV_URL, Models, ModelsDevCatalog, ModelsDevFetch, SyncError,
    UpstreamModels, now_ms,
};

use crate::paths;

/// How long a refreshed layer counts as current.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const MODELS_DEV_FILE: &str = "models.dev.json";
const UPSTREAM_DIR: &str = "upstream";

/// The cached models.dev layer.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsDevEntry<C> {
    /// Last successful check (Unix milliseconds), including `304 Not Modified` answers.
    checked_at: i64,
    /// Source URL; a different URL makes the entry stale.
    url: String,
    /// models.dev provider ids that were requested.
    requested: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    catalog: C,
}

/// Options for [`ModelCatalogCache::sync`].
#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Refresh the models.dev layer.
    pub models_dev: bool,
    /// models.dev catalog URL.
    pub models_dev_url: String,
    /// Refresh provider model lists.
    pub upstream: bool,
    /// Only these providers (all configured providers when empty).
    pub providers: Vec<String>,
    /// Layers younger than this are left alone; `Duration::ZERO` forces a refresh.
    pub max_age: Duration,
    /// Cancels in-flight requests.
    pub cancel: CancellationToken,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            models_dev: true,
            models_dev_url: MODELS_DEV_URL.to_owned(),
            upstream: true,
            providers: Vec::new(),
            max_age: DEFAULT_MAX_AGE,
            cancel: CancellationToken::new(),
        }
    }
}

/// What happened to one layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Fetched and applied; `models` is the number of models in the new layer.
    Updated {
        /// Models in the layer.
        models: usize,
    },
    /// The server confirmed the cached copy.
    NotModified,
    /// Younger than `max_age`; not fetched.
    Fresh,
    /// Not attempted (e.g. no credentials or no list endpoint).
    Skipped(String),
    /// The fetch failed; the previous layer stays in place.
    Failed(String),
}

/// Result of [`ModelCatalogCache::sync`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// The models.dev layer, if requested.
    pub models_dev: Option<SyncOutcome>,
    /// Provider model lists by provider id.
    pub upstream: Vec<(String, SyncOutcome)>,
}

impl SyncReport {
    /// Whether any layer changed.
    pub fn changed(&self) -> bool {
        let updated = |outcome: &SyncOutcome| matches!(outcome, SyncOutcome::Updated { .. });
        self.models_dev.as_ref().is_some_and(updated)
            || self.upstream.iter().any(|(_, outcome)| updated(outcome))
    }
}

/// When each layer was last refreshed, from the cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogStatus {
    /// Last successful models.dev check (Unix milliseconds) and the models it holds.
    pub models_dev: Option<(i64, usize)>,
    /// Per provider: when its list was fetched and how many models it lists.
    pub upstream: Vec<(String, i64, usize)>,
}

/// On-disk storage for the runtime catalog layers.
#[derive(Clone, Debug)]
pub struct ModelCatalogCache {
    dir: PathBuf,
}

impl ModelCatalogCache {
    /// The cache under `data_dir` (see [`paths::models_cache_dir`]).
    pub fn new(data_dir: &Path) -> Self {
        Self::at(paths::models_cache_dir(data_dir))
    }

    /// A cache in `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn models_dev_path(&self) -> PathBuf {
        self.dir.join(MODELS_DEV_FILE)
    }

    fn upstream_path(&self, provider: &str) -> PathBuf {
        let name: String = provider
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(UPSTREAM_DIR).join(format!("{name}.json"))
    }

    /// Applies cached layers to `models` without network access. Provider lists are only
    /// applied to providers whose credentials currently resolve. Unreadable entries are
    /// logged and ignored.
    pub fn load(&self, models: &Models) {
        if let Some(entry) = read_json::<ModelsDevEntry<ModelsDevCatalog>>(&self.models_dev_path())
        {
            models.set_models_dev(Some(entry.catalog));
        }
        for provider in models.providers() {
            let id = provider.id();
            if models.resolve_auth(id).is_none() {
                continue;
            }
            if let Some(list) = read_json::<UpstreamModels>(&self.upstream_path(id)) {
                models.set_upstream(id, Some(list));
            }
        }
    }

    /// When the cached layers were refreshed (reads the cache, no network).
    pub fn status(&self, models: &Models) -> CatalogStatus {
        let models_dev = read_json::<ModelsDevEntry<serde::de::IgnoredAny>>(
            &self.models_dev_path(),
        )
        .map(|entry| {
            let count = models.models_dev().map_or(0, |c| c.model_count());
            (entry.checked_at, count)
        });
        let upstream = models
            .providers()
            .iter()
            .filter_map(|provider| {
                let list = models
                    .upstream(provider.id())
                    .map(|list| (list.fetched_at, list.models.len()))
                    .or_else(|| {
                        read_json::<UpstreamModels>(&self.upstream_path(provider.id()))
                            .map(|list| (list.fetched_at, list.models.len()))
                    })?;
                Some((provider.id().to_owned(), list.0, list.1))
            })
            .collect();
        CatalogStatus {
            models_dev,
            upstream,
        }
    }

    /// Removes the cached list of `provider` and its layer in `models`, e.g. after the
    /// credential changed.
    pub fn clear_upstream(&self, models: &Models, provider: &str) -> Result<()> {
        models.set_upstream(provider, None);
        match fs::remove_file(self.upstream_path(provider)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Refreshes stale layers concurrently, applies them to `models` and persists them.
    pub async fn sync(&self, models: &Models, options: &SyncOptions) -> SyncReport {
        let models_dev = async {
            if options.models_dev {
                Some(self.sync_models_dev(models, options).await)
            } else {
                None
            }
        };
        let upstream = async {
            if !options.upstream {
                return Vec::new();
            }
            let explicit = !options.providers.is_empty();
            let targets: Vec<String> = if explicit {
                options.providers.clone()
            } else {
                models
                    .providers()
                    .iter()
                    .filter(|p| p.api_endpoint().is_some() && models.resolve_auth(p.id()).is_some())
                    .map(|p| p.id().to_owned())
                    .collect()
            };
            let results = join_all(
                targets
                    .iter()
                    .map(|provider| self.sync_upstream(models, provider, options)),
            )
            .await;
            targets.into_iter().zip(results).collect()
        };
        let (models_dev, upstream) = futures::join!(models_dev, upstream);
        SyncReport {
            models_dev,
            upstream,
        }
    }

    async fn sync_models_dev(&self, models: &Models, options: &SyncOptions) -> SyncOutcome {
        let path = self.models_dev_path();
        let requested = models.models_dev_ids();
        if requested.is_empty() {
            return SyncOutcome::Skipped("no provider is linked to models.dev".into());
        }
        // Only the metadata is needed here; skip materializing the catalog.
        let cached = read_json::<ModelsDevEntry<serde::de::IgnoredAny>>(&path).filter(|entry| {
            entry.url == options.models_dev_url && requested.is_subset(&entry.requested)
        });
        if let Some(entry) = &cached
            && is_fresh(entry.checked_at, options.max_age)
        {
            return SyncOutcome::Fresh;
        }
        // Without the catalog loaded, a 304 would leave nothing to apply.
        let etag = cached
            .and_then(|entry| entry.etag)
            .filter(|_| models.models_dev().is_some());

        match models
            .fetch_models_dev(&options.models_dev_url, etag.as_deref(), &options.cancel)
            .await
        {
            Ok(ModelsDevFetch::NotModified) => {
                if let Some(catalog) = models.models_dev() {
                    let entry = ModelsDevEntry {
                        checked_at: now_ms(),
                        url: options.models_dev_url.clone(),
                        requested,
                        etag,
                        catalog: &*catalog,
                    };
                    self.persist(&path, &entry);
                }
                SyncOutcome::NotModified
            }
            Ok(ModelsDevFetch::Updated { catalog, etag }) => {
                let count = catalog.model_count();
                let entry = ModelsDevEntry {
                    checked_at: now_ms(),
                    url: options.models_dev_url.clone(),
                    requested,
                    etag,
                    catalog: &catalog,
                };
                self.persist(&path, &entry);
                models.set_models_dev(Some(catalog));
                SyncOutcome::Updated { models: count }
            }
            Err(error) => failure(error),
        }
    }

    async fn sync_upstream(
        &self,
        models: &Models,
        provider: &str,
        options: &SyncOptions,
    ) -> SyncOutcome {
        let Some(registered) = models.provider(provider) else {
            return SyncOutcome::Skipped("unknown provider".into());
        };
        if registered.api_endpoint().is_none() {
            return SyncOutcome::Skipped("no model list endpoint".into());
        }
        if models.resolve_auth(provider).is_none() {
            return SyncOutcome::Skipped("no credentials".into());
        }
        let path = self.upstream_path(provider);
        let current = models
            .upstream(provider)
            .or_else(|| read_json::<UpstreamModels>(&path).map(std::sync::Arc::new));
        if let Some(current) = &current
            && is_fresh(current.fetched_at, options.max_age)
        {
            if models.upstream(provider).is_none() {
                models.set_upstream(provider, Some((**current).clone()));
            }
            return SyncOutcome::Fresh;
        }
        match models.fetch_upstream(provider, &options.cancel).await {
            Ok(list) if list.models.is_empty() => {
                SyncOutcome::Failed("the provider listed no models".into())
            }
            Ok(list) => {
                self.persist(&path, &list);
                let count = list.models.len();
                models.set_upstream(provider, Some(list));
                SyncOutcome::Updated { models: count }
            }
            Err(error) => failure(error),
        }
    }

    fn persist(&self, path: &Path, value: &impl Serialize) {
        if let Err(error) = write_json(path, value) {
            tracing::warn!(path = %path.display(), %error, "could not write model cache");
        }
    }
}

fn failure(error: SyncError) -> SyncOutcome {
    match error {
        SyncError::Unsupported => SyncOutcome::Skipped("no model list endpoint".into()),
        SyncError::NoCredentials => SyncOutcome::Skipped("no credentials".into()),
        error => SyncOutcome::Failed(error.to_string()),
    }
}

fn is_fresh(checked_at: i64, max_age: Duration) -> bool {
    let age = now_ms().saturating_sub(checked_at);
    age >= 0 && (age as u128) < max_age.as_millis()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not read model cache");
            return None;
        }
    };
    serde_json::from_slice(&bytes)
        .inspect_err(|error| {
            tracing::warn!(path = %path.display(), %error, "ignoring invalid model cache");
        })
        .ok()
}

/// Writes through a temporary file so readers never see a partial file.
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let dir = path.parent().context("cache path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temp, serde_json::to_vec(value)?)
        .with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zp_ai::{EnvLookup, UpstreamModel};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "zp-model-cache-{name}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn env(keys: &'static [&'static str]) -> EnvLookup {
        Arc::new(move |name| keys.contains(&name).then(|| "key".to_owned()))
    }

    fn dev_catalog() -> ModelsDevCatalog {
        let json = serde_json::json!({"anthropic": {"id": "anthropic", "models": {
            "claude-new-1": {"id": "claude-new-1", "tool_call": true, "family": "claude-sonnet",
                             "limit": {"context": 1000, "output": 100}}
        }}});
        ModelsDevCatalog::parse(&serde_json::to_vec(&json).unwrap(), |_| true).unwrap()
    }

    #[test]
    fn layers_round_trip_through_the_cache() {
        let temp = TempDir::new("round-trip");
        let cache = ModelCatalogCache::at(&temp.0);
        let entry = ModelsDevEntry {
            checked_at: now_ms(),
            url: MODELS_DEV_URL.into(),
            requested: BTreeSet::from(["anthropic".to_owned()]),
            etag: Some("\"e\"".into()),
            catalog: dev_catalog(),
        };
        write_json(&cache.models_dev_path(), &entry).unwrap();
        let list = UpstreamModels::new(vec![
            UpstreamModel::new("claude-sonnet-5-5"),
            UpstreamModel::new("claude-new-1"),
        ]);
        write_json(&cache.upstream_path("anthropic"), &list).unwrap();
        write_json(&cache.upstream_path("deepseek"), &list).unwrap();
        fs::write(cache.upstream_path("groq"), "{not json").unwrap();

        // Only anthropic is configured: its list applies, deepseek's cached list does not.
        let models = Models::builtin().with_env(env(&["ANTHROPIC_API_KEY", "GROQ_API_KEY"]));
        let deepseek_before = models.models(Some("deepseek")).len();
        let groq_before = models.models(Some("groq")).len();
        cache.load(&models);

        let anthropic: Vec<String> = models
            .models(Some("anthropic"))
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(anthropic, ["claude-new-1", "claude-sonnet-5-5"]);
        assert!(models.models_dev().is_some());
        assert_eq!(models.models(Some("deepseek")).len(), deepseek_before);
        assert_eq!(
            models.models(Some("groq")).len(),
            groq_before,
            "corrupt entry ignored"
        );

        cache.clear_upstream(&models, "anthropic").unwrap();
        assert!(!cache.upstream_path("anthropic").exists());
        assert!(models.upstream("anthropic").is_none());
        cache.clear_upstream(&models, "anthropic").unwrap();
    }

    #[tokio::test]
    async fn fresh_layers_are_not_fetched() {
        let temp = TempDir::new("fresh");
        let cache = ModelCatalogCache::at(&temp.0);
        let models = Models::builtin().with_env(env(&["ANTHROPIC_API_KEY"]));
        let entry = ModelsDevEntry {
            checked_at: now_ms(),
            // Unroutable on purpose: a fetch would fail the test.
            url: "http://127.0.0.1:9/api.json".into(),
            requested: models.models_dev_ids(),
            etag: None,
            catalog: dev_catalog(),
        };
        write_json(&cache.models_dev_path(), &entry).unwrap();
        write_json(
            &cache.upstream_path("anthropic"),
            &UpstreamModels::new(vec![UpstreamModel::new("claude-sonnet-5-5")]),
        )
        .unwrap();

        let options = SyncOptions {
            models_dev_url: entry.url.clone(),
            ..SyncOptions::default()
        };
        let report = cache.sync(&models, &options).await;
        assert_eq!(report.models_dev, Some(SyncOutcome::Fresh));
        assert_eq!(
            report.upstream,
            [("anthropic".to_owned(), SyncOutcome::Fresh)]
        );
        assert!(!report.changed());
        // A fresh cached list is applied even without an explicit load.
        assert_eq!(models.models(Some("anthropic")).len(), 1);

        // Explicitly requested providers without credentials are reported.
        let options = SyncOptions {
            models_dev: false,
            providers: vec!["deepseek".into(), "nope".into()],
            ..SyncOptions::default()
        };
        let report = cache.sync(&models, &options).await;
        assert_eq!(
            report.upstream,
            [
                (
                    "deepseek".to_owned(),
                    SyncOutcome::Skipped("no credentials".into())
                ),
                (
                    "nope".to_owned(),
                    SyncOutcome::Skipped("unknown provider".into())
                ),
            ]
        );
    }

    #[test]
    fn freshness() {
        assert!(is_fresh(now_ms() - 1_000, DEFAULT_MAX_AGE));
        assert!(!is_fresh(now_ms() - 1_000, Duration::ZERO));
        assert!(!is_fresh(0, DEFAULT_MAX_AGE));
        assert!(!is_fresh(now_ms() + 60_000, DEFAULT_MAX_AGE), "clock skew");
    }

    #[test]
    fn provider_ids_are_safe_file_names() {
        let cache = ModelCatalogCache::at("/c");
        assert_eq!(
            cache.upstream_path("../x/y"),
            Path::new("/c/upstream/.._x_y.json")
        );
    }
}
