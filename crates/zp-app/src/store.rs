//! App-wide stores held as GPUI globals: data paths, the persisted UI state and the model
//! registry.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, Global, Task};
use std::sync::Arc;

use zp_ai::{HttpProvider, Model, Models, Provider};
use zp_core::app_state::AppState;

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

/// Providers, credentials and model catalogs shared by every window.
pub struct AppModels {
    pub models: Models,
    /// Name new sessions with a short model-generated title.
    pub generate_titles: bool,
}

impl Global for AppModels {}

impl AppModels {
    /// Built-in providers with the cached catalog layers applied.
    ///
    /// `ZP_BASE_URL_<PROVIDER>` (e.g. `ZP_BASE_URL_DEEPSEEK`) points a provider at another
    /// endpoint such as a proxy or a local mock server.
    pub fn init(cx: &mut App) {
        let mut models = Models::builtin();
        if let Some(dir) = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone()) {
            zp_core::model_catalog::ModelCatalogCache::new(&dir).load(&models);
        }
        apply_base_url_overrides(&mut models);
        cx.set_global(Self {
            models,
            generate_titles: true,
        });
    }

    /// Replaces the registry (tests use a scripted provider).
    #[cfg(any(test, feature = "visual-tests"))]
    pub fn set(models: Models, generate_titles: bool, cx: &mut App) {
        cx.set_global(Self {
            models,
            generate_titles,
        });
    }

    /// The model new conversations use: the default of the first provider with
    /// credentials.
    pub fn default_model(cx: &App) -> Option<Model> {
        let models = &cx.try_global::<Self>()?.models;
        models
            .available_providers()
            .iter()
            .find_map(|provider| models.default_model(provider.id()))
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
