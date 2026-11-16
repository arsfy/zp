//! App-wide stores held as GPUI globals: data paths, the persisted UI state, the user's
//! settings and the model registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Global, Task};
use zp_ai::{HttpProvider, Model, Models, Provider};
use zp_core::app_state::AppState;
use zp_core::credentials::Credentials;
use zp_core::model_catalog::{ModelCatalogCache, SyncOptions, SyncReport};
use zp_core::settings::Settings;

/// Delay before writing state changes, so bursts (window drags) cause one write.
const SAVE_DELAY: Duration = Duration::from_millis(500);

/// Where ZP keeps its data; `None` when no data directory is available.
#[derive(Clone, Debug, Default)]
pub struct AppPaths {
    pub data_dir: Option<PathBuf>,
}

impl Global for AppPaths {}

impl AppPaths {
    pub fn init(data_dir: Option<PathBuf>, cx: &mut App) {
        cx.set_global(Self { data_dir });
    }

    /// The snapshot store of the data directory.
    pub fn blobs(cx: &App) -> Option<zp_core::checkpoint::BlobStore> {
        let paths = cx.try_global::<Self>()?;
        paths
            .data_dir
            .as_deref()
            .map(zp_core::checkpoint::BlobStore::in_data_dir)
    }

    pub fn sessions_dir(cx: &App) -> Option<PathBuf> {
        let paths = cx.try_global::<Self>()?;
        paths.data_dir.as_deref().map(zp_core::paths::sessions_dir)
    }
}

/// The persisted UI state ([`AppState`]) with debounced saving.
pub struct AppStore {
    path: Option<PathBuf>,
    state: AppState,
    save_task: Option<Task<()>>,
}

impl Global for AppStore {}

impl AppStore {
    /// Loads the state from `path` (or starts empty without one).
    pub fn init(path: Option<PathBuf>, cx: &mut App) {
        let state = path.as_deref().map(AppState::load).unwrap_or_default();
        cx.set_global(Self {
            path,
            state,
            save_task: None,
        });
    }

    /// The current state.
    pub fn state(cx: &App) -> &AppState {
        &cx.global::<Self>().state
    }

    /// Changes the state and schedules a save. Observers of the global re-render.
    pub fn update<R>(cx: &mut App, change: impl FnOnce(&mut AppState) -> R) -> R {
        let store = cx.global_mut::<Self>();
        let result = change(&mut store.state);
        if store.save_task.is_none() && store.path.is_some() {
            let task = cx.spawn(async move |cx| {
                cx.background_executor().timer(SAVE_DELAY).await;
                cx.update(Self::flush);
            });
            cx.global_mut::<Self>().save_task = Some(task);
        }
        result
    }

    /// Writes pending changes now.
    pub fn flush(cx: &mut App) {
        let store = cx.global_mut::<Self>();
        if let Some(task) = store.save_task.take() {
            // May be the task running this call; let it finish.
            task.detach();
        }
        if let Some(path) = &store.path
            && let Err(error) = store.state.save(path)
        {
            tracing::warn!(%error, "could not save the UI state");
        }
    }
}

/// The user's settings (`~/.zp/settings.json`), with saving and applying changes.
pub struct AppSettings {
    path: Option<PathBuf>,
    settings: Settings,
}

impl Global for AppSettings {}

impl AppSettings {
    pub fn init(data_dir: Option<&Path>, cx: &mut App) {
        let path = data_dir.map(zp_core::paths::settings_file);
        let settings = path
            .as_deref()
            .map(Settings::load_or_default)
            .unwrap_or_default();
        cx.set_global(Self { path, settings });
    }

    /// The current settings.
    pub fn get(cx: &App) -> &Settings {
        &cx.global::<Self>().settings
    }

    /// Changes the settings, saves them and applies them to the running app.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) -> anyhow::Result<()> {
        let this = cx.global_mut::<Self>();
        // Pick up edits made to the file meanwhile.
        if let Some(path) = &this.path
            && let Ok(current) = Settings::load(path)
        {
            this.settings = current;
        }
        change(&mut this.settings);
        let result = match &this.path {
            Some(path) => this.settings.save(path),
            None => Ok(()),
        };
        crate::apply_settings(cx);
        result
    }
}

/// State of model catalog syncing, for the settings window.
#[derive(Clone, Debug, Default)]
pub struct CatalogSync {
    /// A sync is running.
    pub running: bool,
    /// The last finished sync: when, and what happened.
    pub last: Option<(i64, SyncReport)>,
    /// Requested refreshes (providers, forced), for tests.
    pub requests: Vec<(Vec<String>, bool)>,
    /// Whether refreshes reach the network (off in tests).
    pub offline: bool,
}

/// Providers, credentials and model catalogs shared by every window.
pub struct AppModels {
    pub models: Models,
    /// Stored API keys.
    pub credentials: Arc<Credentials>,
    /// Name new sessions with a short model-generated title.
    pub generate_titles: bool,
    pub sync: CatalogSync,
}

impl Global for AppModels {}

impl AppModels {
    /// Built-in and custom providers with keychain credentials and the cached catalog
    /// layers applied.
    ///
    /// `ZP_BASE_URL_<PROVIDER>` (e.g. `ZP_BASE_URL_DEEPSEEK`) points a provider at another
    /// endpoint such as a proxy or a local mock server.
    pub fn init(cx: &mut App) {
        let data_dir = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone());
        let credentials = Arc::new(match &data_dir {
            Some(dir) => Credentials::keychain(dir),
            None => Credentials::in_memory(),
        });
        let mut models = Models::builtin().with_credentials(credentials.clone());
        let settings = AppSettings::get(cx).clone();
        zp_core::providers::register_custom_providers(&models, &settings.providers);
        if let Some(dir) = &data_dir {
            ModelCatalogCache::new(dir).load(&models);
        }
        apply_base_url_overrides(&mut models);
        cx.set_global(Self {
            models,
            credentials,
            generate_titles: settings.generate_titles.unwrap_or(true),
            sync: CatalogSync::default(),
        });
    }

    /// Replaces the registry (tests use a scripted provider); keys are kept in memory.
    #[cfg(any(test, feature = "visual-tests"))]
    pub fn set(models: Models, generate_titles: bool, cx: &mut App) {
        let credentials = Arc::new(Credentials::in_memory());
        cx.set_global(Self {
            models: models.with_credentials(credentials.clone()),
            credentials,
            generate_titles,
            sync: CatalogSync {
                offline: true,
                ..CatalogSync::default()
            },
        });
    }

    /// The model new conversations use: the one in the settings if it is available,
    /// else the default of the first provider with credentials.
    pub fn default_model(cx: &App) -> Option<Model> {
        let models = &cx.try_global::<Self>()?.models;
        let configured = AppSettings::get(cx)
            .default_model
            .as_deref()
            .and_then(|spec| models.find(spec))
            .filter(|model| models.resolve_auth(&model.provider).is_some());
        configured.or_else(|| {
            models
                .available_providers()
                .iter()
                .find_map(|provider| models.default_model(provider.id()))
        })
    }

    /// Whether `provider` can be used (has a key, or needs none).
    pub fn has_credentials(provider: &str, cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|app| app.models.resolve_auth(provider).is_some())
    }

    /// The model catalog cache, if there is a data directory.
    pub fn catalog_cache(cx: &App) -> Option<ModelCatalogCache> {
        let dir = cx.try_global::<AppPaths>()?.data_dir.clone()?;
        Some(ModelCatalogCache::new(&dir))
    }

    /// Refreshes the catalog in the background: everything (`providers` empty) or only
    /// some providers' model lists. `force` ignores cache freshness.
    pub fn sync(cx: &mut App, providers: Vec<String>, force: bool) {
        let Some(cache) = Self::catalog_cache(cx) else {
            return;
        };
        let state = &mut cx.global_mut::<Self>().sync;
        state.requests.push((providers.clone(), force));
        if state.offline {
            return;
        }
        let models = cx.global::<Self>().models.clone();
        let options = SyncOptions {
            models_dev: providers.is_empty(),
            providers,
            max_age: if force {
                Duration::ZERO
            } else {
                SyncOptions::default().max_age
            },
            ..SyncOptions::default()
        };
        cx.global_mut::<Self>().sync.running = true;
        cx.refresh_windows();
        let task = gpui_tokio::Tokio::spawn(cx, async move { cache.sync(&models, &options).await });
        cx.spawn(async move |cx| {
            let report = task.await;
            cx.update(|cx| {
                let sync = &mut cx.global_mut::<Self>().sync;
                sync.running = false;
                match report {
                    Ok(report) => {
                        tracing::info!(?report, "model catalog synced");
                        sync.last = Some((zp_ai::now_ms(), report));
                    }
                    Err(error) => tracing::warn!(%error, "model catalog sync stopped"),
                }
                cx.refresh_windows();
            });
        })
        .detach();
    }

    /// After a key change: drops the provider's cached model list and fetches a new one.
    pub fn credential_changed(cx: &mut App, provider: &str) {
        let models = cx.global::<Self>().models.clone();
        if let Some(cache) = Self::catalog_cache(cx) {
            if let Err(error) = cache.clear_upstream(&models, provider) {
                tracing::warn!(%error, provider, "could not clear the model list");
            }
        } else {
            models.set_upstream(provider, None);
        }
        if models.resolve_auth(provider).is_some() {
            Self::sync(cx, vec![provider.to_owned()], true);
        }
        cx.refresh_windows();
    }
}

/// Applies `ZP_BASE_URL_<PROVIDER>` environment overrides.
fn apply_base_url_overrides(models: &mut Models) {
    let overrides: Vec<(Arc<dyn Provider>, String)> = models
        .providers()
        .iter()
        .filter_map(|provider| {
            let var = format!(
                "ZP_BASE_URL_{}",
                provider.id().to_uppercase().replace('-', "_")
            );
            let url = std::env::var(var)
                .ok()
                .filter(|url| !url.trim().is_empty())?;
            Some((provider.clone(), url))
        })
        .collect();
    for (provider, url) in overrides {
        tracing::info!(provider = provider.id(), %url, "overriding base URL");
        let catalog: Vec<Model> = models
            .models(Some(provider.id()))
            .into_iter()
            .map(|mut model| {
                model.base_url.clone_from(&url);
                model
            })
            .collect();
        let mut replacement = HttpProvider::new(
            provider.id(),
            provider.name(),
            provider.api_key_env_vars().to_vec(),
            catalog,
        );
        if let Some(id) = provider.default_model_id() {
            replacement = replacement.with_default_model(id);
        }
        if let Some((api, _)) = provider.api_endpoint() {
            replacement = replacement.with_api_endpoint(api, url.clone());
        }
        if let Some(id) = provider.models_dev_id() {
            replacement = replacement.with_models_dev_id(id);
        }
        if let Some(flow) = provider.oauth() {
            replacement = replacement.with_oauth(flow);
        }
        if !provider.accepts_api_key() {
            replacement = replacement.oauth_only();
        }
        models.set_provider(Arc::new(replacement));
    }
}

/// How deleted sessions are removed: the system trash by default; tests delete directly.
#[derive(Clone, Copy)]
pub struct FileTrash(pub fn(&std::path::Path) -> anyhow::Result<()>);

impl Global for FileTrash {}

impl FileTrash {
    /// Moves files to the platform trash.
    pub fn system() -> Self {
        Self(|path| trash::delete(path).map_err(|error| anyhow::anyhow!("{error}")))
    }
}
