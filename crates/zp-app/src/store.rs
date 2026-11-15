//! App-wide stores held as GPUI globals: data paths, the persisted UI state and the model
//! registry.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, Global, Task};
use zp_ai::{Model, Models};
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
}

impl Global for AppModels {}

impl AppModels {
    /// Built-in providers with the cached catalog layers applied.
    pub fn init(cx: &mut App) {
        let models = Models::builtin();
        if let Some(dir) = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone()) {
            zp_core::model_catalog::ModelCatalogCache::new(&dir).load(&models);
        }
        cx.set_global(Self { models });
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
