//! A collection of providers that routes requests and resolves credentials.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, PoisonError, RwLock};

use tokio_util::sync::CancellationToken;

use crate::auth::{
    AuthSource, CredentialStore, EnvLookup, MemoryCredentialStore, ResolvedAuth, process_env,
};
use crate::catalog::builtin_providers;
use crate::event::{AssistantMessageStream, collect_message, error_stream};
use crate::model::Model;
use crate::options::StreamOptions;
use crate::provider::Provider;
use crate::sync::{
    ModelsDevCatalog, ModelsDevFetch, SyncError, UpstreamModels, fetch_models_dev, resolve_models,
};
use crate::types::{AssistantMessage, Context};

/// Registered providers plus credential resolution.
///
/// Model lists are each provider's catalog with the models.dev and upstream layers
/// applied (see [`crate::sync`]). Layers live behind a shared lock, so clones of a
/// `Models` observe each other's [`Self::set_models_dev`] / [`Self::set_upstream`] calls.
#[derive(Clone)]
pub struct Models {
    providers: Vec<Arc<dyn Provider>>,
    credentials: Arc<dyn CredentialStore>,
    env: EnvLookup,
    layers: Arc<RwLock<Layers>>,
}

/// Runtime catalog layers and the per-provider lists resolved from them.
#[derive(Default)]
struct Layers {
    models_dev: Option<Arc<ModelsDevCatalog>>,
    upstream: HashMap<String, Arc<UpstreamModels>>,
    resolved: HashMap<String, Arc<[Model]>>,
    /// Bumped on every change so stale resolutions are not cached.
    generation: u64,
}

impl Layers {
    fn changed(&mut self) {
        self.resolved.clear();
        self.generation += 1;
    }
}

impl Default for Models {
    fn default() -> Self {
        Self::new()
    }
}

impl Models {
    /// An empty collection reading keys from the process environment.
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            credentials: Arc::new(MemoryCredentialStore::new()),
            env: process_env(),
            layers: Arc::default(),
        }
    }

    /// A collection with every built-in provider.
    pub fn builtin() -> Self {
        let mut models = Self::new();
        for provider in builtin_providers() {
            models.set_provider(provider);
        }
        models
    }

    /// Uses `store` for stored credentials.
    pub fn with_credentials(mut self, store: Arc<dyn CredentialStore>) -> Self {
        self.credentials = store;
        self
    }

    /// Replaces environment lookup (for tests).
    pub fn with_env(mut self, env: EnvLookup) -> Self {
        self.env = env;
        self
    }

    /// Registers a provider, replacing one with the same id.
    pub fn set_provider(&mut self, provider: Arc<dyn Provider>) {
        self.write_layers().changed();
        match self.providers.iter_mut().find(|p| p.id() == provider.id()) {
            Some(slot) => *slot = provider,
            None => self.providers.push(provider),
        }
    }

    fn read_layers(&self) -> std::sync::RwLockReadGuard<'_, Layers> {
        self.layers.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_layers(&self) -> std::sync::RwLockWriteGuard<'_, Layers> {
        self.layers.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The effective model list of `provider`.
    fn provider_models(&self, provider: &Arc<dyn Provider>) -> Arc<[Model]> {
        let (resolved, generation) = {
            let layers = self.read_layers();
            if let Some(models) = layers.resolved.get(provider.id()) {
                return models.clone();
            }
            let dev = provider
                .models_dev_id()
                .zip(layers.models_dev.as_deref())
                .and_then(|(id, catalog)| catalog.provider(id));
            let upstream = layers.upstream.get(provider.id()).map(Arc::as_ref);
            if dev.is_none() && upstream.is_none() {
                return provider.models();
            }
            let resolved: Arc<[Model]> = resolve_models(provider.as_ref(), dev, upstream).into();
            (resolved, layers.generation)
        };
        let mut layers = self.write_layers();
        if layers.generation == generation {
            layers
                .resolved
                .insert(provider.id().to_owned(), resolved.clone());
        }
        resolved
    }

    /// Replaces (or with `None` removes) the models.dev layer.
    pub fn set_models_dev(&self, catalog: Option<ModelsDevCatalog>) {
        let mut layers = self.write_layers();
        layers.models_dev = catalog.map(Arc::new);
        layers.changed();
    }

    /// The models.dev layer, if any.
    pub fn models_dev(&self) -> Option<Arc<ModelsDevCatalog>> {
        self.read_layers().models_dev.clone()
    }

    /// models.dev ids of the registered providers.
    pub fn models_dev_ids(&self) -> BTreeSet<String> {
        self.providers
            .iter()
            .filter_map(|p| p.models_dev_id().map(str::to_owned))
            .collect()
    }

    /// Replaces (or with `None` removes) the upstream model list of `provider`.
    pub fn set_upstream(&self, provider: &str, models: Option<UpstreamModels>) {
        let mut layers = self.write_layers();
        match models {
            Some(models) => layers
                .upstream
                .insert(provider.to_owned(), Arc::new(models)),
            None => layers.upstream.remove(provider),
        };
        layers.changed();
    }

    /// The upstream model list applied to `provider`, if any.
    pub fn upstream(&self, provider: &str) -> Option<Arc<UpstreamModels>> {
        self.read_layers().upstream.get(provider).cloned()
    }

    /// Fetches the models.dev catalog for the registered providers. Pass the ETag of the
    /// copy you already have to allow a `NotModified` answer. Does not apply the result.
    pub async fn fetch_models_dev(
        &self,
        url: &str,
        etag: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<ModelsDevFetch, SyncError> {
        let ids = self.models_dev_ids();
        fetch_models_dev(url, etag, |id| ids.contains(id), cancel).await
    }

    /// Lists the models `provider` offers to the configured credential. Does not apply
    /// the result.
    pub async fn fetch_upstream(
        &self,
        provider: &str,
        cancel: &CancellationToken,
    ) -> Result<UpstreamModels, SyncError> {
        let registered = self
            .provider(provider)
            .ok_or_else(|| SyncError::Failed(format!("unknown provider {provider}")))?;
        let auth = self
            .resolve_auth(provider)
            .ok_or(SyncError::NoCredentials)?;
        let models = registered.list_models(auth.api_key, cancel.clone()).await?;
        Ok(UpstreamModels::new(models))
    }

    /// Registered providers in registration order.
    pub fn providers(&self) -> &[Arc<dyn Provider>] {
        &self.providers
    }

    /// The provider with `id`.
    pub fn provider(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.iter().find(|provider| provider.id() == id)
    }

    /// All models, optionally limited to one provider.
    pub fn models(&self, provider: Option<&str>) -> Vec<Model> {
        self.providers
            .iter()
            .filter(|p| provider.is_none_or(|id| p.id() == id))
            .flat_map(|p| self.provider_models(p).to_vec())
            .collect()
    }

    /// The model `id` of `provider`.
    pub fn model(&self, provider: &str, id: &str) -> Option<Model> {
        self.provider_models(self.provider(provider)?)
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// Looks up `provider/model-id`, or a bare model id (first provider wins). Model ids
    /// may themselves contain `/`, e.g. `openrouter/anthropic/claude-sonnet-5.5`.
    pub fn find(&self, spec: &str) -> Option<Model> {
        if let Some((provider, id)) = spec.split_once('/')
            && let Some(model) = self.model(provider, id)
        {
            return Some(model);
        }
        self.providers.iter().find_map(|p| {
            self.provider_models(p)
                .iter()
                .find(|m| m.id == spec)
                .cloned()
        })
    }

    /// The provider's default model, if it is in the effective list.
    pub fn default_model(&self, provider: &str) -> Option<Model> {
        let provider = self.provider(provider)?;
        let id = provider.default_model_id()?;
        self.provider_models(provider)
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// Resolves credentials for `provider` without making a request.
    pub fn resolve_auth(&self, provider: &str) -> Option<ResolvedAuth> {
        let registered = self.provider(provider)?;
        if let Some(key) = self.credentials.api_key(provider) {
            return Some(ResolvedAuth {
                api_key: Some(key),
                source: AuthSource::Stored,
            });
        }
        for name in registered.api_key_env_vars() {
            if let Some(key) = (self.env)(name) {
                return Some(ResolvedAuth {
                    api_key: Some(key),
                    source: AuthSource::Env(name.clone()),
                });
            }
        }
        (!registered.requires_api_key()).then_some(ResolvedAuth {
            api_key: None,
            source: AuthSource::NotRequired,
        })
    }

    /// Providers whose credentials resolve.
    pub fn available_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .iter()
            .filter(|p| self.resolve_auth(p.id()).is_some())
            .cloned()
            .collect()
    }

    /// Streams a response from `model`, resolving the API key unless one is given.
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        mut options: StreamOptions,
    ) -> AssistantMessageStream {
        let skeleton = || AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);
        let Some(provider) = self.provider(&model.provider) else {
            return error_stream(skeleton(), format!("Unknown provider: {}", model.provider));
        };
        if options.api_key.is_none() {
            match self.resolve_auth(&model.provider) {
                Some(auth) => options.api_key = auth.api_key,
                None => {
                    let hint = match provider.api_key_env_vars() {
                        [] => String::new(),
                        vars => format!(". Set {}", vars.join(" or ")),
                    };
                    return error_stream(
                        skeleton(),
                        format!("No API key for provider {}{hint}", model.provider),
                    );
                }
            }
        }
        provider.stream(model, context, options)
    }

    /// Streams a response and returns the final message.
    pub async fn complete(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessage {
        collect_message(self.stream(model, context, options)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faux::{FauxProvider, FauxResponse};
    use crate::types::{Message, StopReason};
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> EnvLookup {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Arc::new(move |name| map.get(name).cloned())
    }

    #[test]
    fn finds_models_by_spec() {
        let models = Models::builtin().with_env(env(&[]));
        assert_eq!(
            models.find("anthropic/claude-sonnet-5-5").unwrap().provider,
            "anthropic"
        );
        let routed = models
            .find("openrouter/anthropic/claude-sonnet-5.5")
            .unwrap();
        assert_eq!(
            (routed.provider.as_str(), routed.id.as_str()),
            ("openrouter", "anthropic/claude-sonnet-5.5")
        );
        assert_eq!(
            models.find("claude-sonnet-5-5").unwrap().provider,
            "anthropic"
        );
        assert!(models.find("nope/nothing").is_none());
        assert_eq!(
            models.default_model("deepseek").unwrap().id,
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn auth_order_is_store_then_env() {
        let store = Arc::new(MemoryCredentialStore::new());
        let models = Models::builtin()
            .with_env(env(&[("ANTHROPIC_API_KEY", "from-env")]))
            .with_credentials(store.clone());
        let auth = models.resolve_auth("anthropic").unwrap();
        assert_eq!(auth.source, AuthSource::Env("ANTHROPIC_API_KEY".into()));
        assert_eq!(auth.api_key.as_deref(), Some("from-env"));

        store.set("anthropic", "stored");
        let auth = models.resolve_auth("anthropic").unwrap();
        assert_eq!(auth.source, AuthSource::Stored);
        assert!(format!("{auth:?}").contains("<redacted>"));

        assert!(models.resolve_auth("deepseek").is_none());
        let available: Vec<_> = models
            .available_providers()
            .iter()
            .map(|p| p.id().to_owned())
            .collect();
        assert_eq!(available, ["anthropic"]);
    }

    #[test]
    fn layers_apply_to_clones_and_can_be_removed() {
        use crate::sync::UpstreamModel;

        let models = Models::builtin().with_env(env(&[]));
        let clone = models.clone();
        let builtin = models.models(Some("anthropic")).len();
        assert!(models.models_dev_ids().contains("zai-coding-plan"));

        models.set_upstream(
            "anthropic",
            Some(UpstreamModels::new(vec![
                UpstreamModel::new("claude-sonnet-5-5"),
                UpstreamModel::new("claude-next-1"),
            ])),
        );
        let ids: Vec<String> = clone
            .models(Some("anthropic"))
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["claude-next-1", "claude-sonnet-5-5"]);
        let next = clone.find("anthropic/claude-next-1").unwrap();
        assert_eq!(next.api, crate::model::Api::AnthropicMessages);
        assert_eq!(next.base_url, "https://api.anthropic.com");
        assert!(clone.default_model("anthropic").is_some());
        // Other providers are untouched.
        assert_eq!(
            models.models(Some("deepseek")).len(),
            crate::catalog::builtin_providers()[2].models().len()
        );

        models.set_upstream("anthropic", None);
        assert_eq!(clone.models(Some("anthropic")).len(), builtin);
    }

    #[tokio::test]
    async fn fetching_upstream_needs_credentials() {
        let models = Models::builtin().with_env(env(&[]));
        let cancel = CancellationToken::new();
        assert_eq!(
            models.fetch_upstream("anthropic", &cancel).await,
            Err(SyncError::NoCredentials)
        );
        assert!(matches!(
            models.fetch_upstream("nope", &cancel).await,
            Err(SyncError::Failed(_))
        ));
        let mut models = models;
        models.set_provider(Arc::new(FauxProvider::default()));
        assert_eq!(
            models.fetch_upstream("faux", &cancel).await,
            Err(SyncError::Unsupported)
        );
    }

    #[tokio::test]
    async fn missing_key_fails_without_a_request() {
        let models = Models::builtin().with_env(env(&[]));
        let model = models.default_model("anthropic").unwrap();
        let message = models
            .complete(&model, &Context::default(), StreamOptions::default())
            .await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("No API key for provider anthropic. Set ANTHROPIC_API_KEY")
        );
    }

    #[tokio::test]
    async fn routes_to_registered_provider() {
        let faux = FauxProvider::default();
        faux.push(FauxResponse::text("hi"));
        let mut models = Models::new().with_env(env(&[]));
        models.set_provider(Arc::new(faux.clone()));
        let context = Context {
            messages: vec![Message::user("hello")],
            ..Context::default()
        };
        let message = models
            .complete(&faux.model(), &context, StreamOptions::default())
            .await;
        assert_eq!(message.text(), "hi");
        assert_eq!(faux.requests()[0].messages.len(), 1);
    }
}
