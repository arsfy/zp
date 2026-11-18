//! A collection of providers that routes requests and resolves credentials.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, PoisonError, RwLock};

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::auth::{
    AuthSource, Credential, CredentialStore, EnvLookup, MemoryCredentialStore, OAuthCredential,
    RequestAuth, ResolvedAuth, process_env,
};
use crate::catalog::builtin_providers;
use crate::event::{AssistantMessageStream, collect_message, error_stream};
use crate::model::Model;
use crate::oauth::{LoginInteraction, OAuthFlow};
use crate::options::StreamOptions;
use crate::provider::Provider;
use crate::sync::{
    ModelsDevCatalog, ModelsDevFetch, Reference, SyncError, UpstreamModels, fetch_models_dev,
    resolve_models,
};
use crate::types::{AssistantMessage, Context};

/// Where a model in [`Models::models`] comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSource {
    /// The provider's built-in catalog (possibly with metadata from models.dev).
    BuiltIn,
    /// Added from models.dev.
    ModelsDev,
    /// Only in the provider's own model list.
    Upstream,
}

impl ModelSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::ModelsDev => "models.dev",
            Self::Upstream => "provider list",
        }
    }
}

/// Registered providers plus credential resolution.
///
/// Model lists are each provider's catalog with the models.dev and upstream layers
/// applied (see [`crate::sync`]). Providers and layers live behind shared locks, so clones
/// of a `Models` observe each other's changes (a provider added in the settings is usable
/// by running conversations).
#[derive(Clone)]
pub struct Models {
    providers: Arc<RwLock<Vec<Arc<dyn Provider>>>>,
    credentials: Arc<dyn CredentialStore>,
    env: EnvLookup,
    layers: Arc<RwLock<Layers>>,
    /// One lock per provider so concurrent requests refresh an OAuth token only once.
    refresh_locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
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
            providers: Arc::default(),
            credentials: Arc::new(MemoryCredentialStore::new()),
            env: process_env(),
            layers: Arc::default(),
            refresh_locks: Arc::default(),
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
        self.register(provider);
    }

    /// Registers a provider, replacing one with the same id (also for clones).
    pub fn register(&self, provider: Arc<dyn Provider>) {
        self.write_layers().changed();
        let mut providers = self
            .providers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        match providers.iter_mut().find(|p| p.id() == provider.id()) {
            Some(slot) => *slot = provider,
            None => providers.push(provider),
        }
    }

    /// Removes provider `id` and its upstream layer. Returns whether it was registered.
    pub fn remove_provider(&self, id: &str) -> bool {
        let removed = {
            let mut providers = self
                .providers
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let before = providers.len();
            providers.retain(|p| p.id() != id);
            providers.len() != before
        };
        let mut layers = self.write_layers();
        layers.upstream.remove(id);
        layers.changed();
        removed
    }

    /// Where `model` comes from: the built-in catalog, models.dev or only the provider's
    /// own list.
    pub fn model_source(&self, model: &Model) -> ModelSource {
        let Some(provider) = self.provider(&model.provider) else {
            return ModelSource::BuiltIn;
        };
        if provider.models().iter().any(|m| m.id == model.id) {
            return ModelSource::BuiltIn;
        }
        let layers = self.read_layers();
        let in_dev = provider
            .models_dev_id()
            .zip(layers.models_dev.as_deref())
            .and_then(|(id, catalog)| catalog.provider(id))
            .is_some_and(|dev| dev.models.contains_key(&model.id));
        if in_dev {
            ModelSource::ModelsDev
        } else {
            ModelSource::Upstream
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
            let reference = Reference {
                catalog: layers.models_dev.as_deref(),
                builtin: if upstream.is_some() {
                    self.providers()
                        .iter()
                        .filter(|other| other.id() != provider.id())
                        .flat_map(|other| other.models().iter().cloned().collect::<Vec<_>>())
                        .collect()
                } else {
                    Vec::new()
                },
            };
            let resolved: Arc<[Model]> =
                resolve_models(provider.as_ref(), dev, upstream, &reference).into();
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
        self.providers()
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
        if self.resolve_auth(provider).is_none() {
            return Err(SyncError::NoCredentials);
        }
        let auth = self
            .request_auth(provider, cancel)
            .await
            .map_err(SyncError::Failed)?;
        let models = registered.list_models(auth.api_key, cancel.clone()).await?;
        Ok(UpstreamModels::new(models))
    }

    /// Registered providers in registration order.
    pub fn providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The provider with `id`.
    pub fn provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|provider| provider.id() == id)
            .cloned()
    }

    /// All models, optionally limited to one provider. A provider signed in with an
    /// account lists only the models the account can use.
    pub fn models(&self, provider: Option<&str>) -> Vec<Model> {
        self.providers()
            .iter()
            .filter(|p| provider.is_none_or(|id| p.id() == id))
            .flat_map(|p| {
                let models = self.provider_models(p).to_vec();
                match (p.oauth(), self.credentials.credential(p.id())) {
                    (Some(flow), Some(Credential::OAuth(credential))) => {
                        flow.filter_models(&credential, models)
                    }
                    _ => models,
                }
            })
            .collect()
    }

    /// The model `id` of `provider`.
    pub fn model(&self, provider: &str, id: &str) -> Option<Model> {
        self.provider_models(&self.provider(provider)?)
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
        self.providers().iter().find_map(|p| {
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
        self.provider_models(&provider)
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// Resolves credentials for `provider` without making a request. A sign-in resolves
    /// to its current access token, which may have expired ([`Self::request_auth`]
    /// refreshes it).
    pub fn resolve_auth(&self, provider: &str) -> Option<ResolvedAuth> {
        let registered = self.provider(provider)?;
        match self.credentials.credential(provider) {
            Some(Credential::ApiKey(key)) => {
                return Some(ResolvedAuth {
                    api_key: Some(key),
                    source: AuthSource::Stored,
                });
            }
            Some(Credential::OAuth(credential)) if registered.oauth().is_some() => {
                return Some(ResolvedAuth {
                    api_key: Some(credential.access),
                    source: AuthSource::OAuth,
                });
            }
            _ => {}
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

    /// The OAuth flow of `provider`, if it supports signing in.
    pub fn oauth_flow(&self, provider: &str) -> Option<Arc<dyn OAuthFlow>> {
        self.provider(provider)?.oauth()
    }

    /// Whether `provider` is signed in (rather than using an API key).
    pub fn is_signed_in(&self, provider: &str) -> bool {
        self.resolve_auth(provider)
            .is_some_and(|auth| auth.source == AuthSource::OAuth)
    }

    /// Signs in to `provider` and stores the tokens.
    pub async fn login(&self, provider: &str, interaction: LoginInteraction) -> Result<(), String> {
        let flow = self
            .oauth_flow(provider)
            .ok_or_else(|| format!("{provider} does not support signing in"))?;
        let credential = flow.login(interaction).await?;
        self.credentials
            .store(provider, &Credential::OAuth(credential))
            .map_err(|error| format!("could not save the sign-in: {error}"))
    }

    fn refresh_lock(&self, provider: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(provider.to_owned())
            .or_default()
            .clone()
    }

    /// What a request to `provider` authenticates with, refreshing an expired sign-in
    /// first (and storing the new tokens).
    pub async fn request_auth(
        &self,
        provider: &str,
        cancel: &CancellationToken,
    ) -> Result<RequestAuth, String> {
        let registered = self
            .provider(provider)
            .ok_or_else(|| format!("Unknown provider: {provider}"))?;
        if let Some(flow) = registered.oauth()
            && let Some(Credential::OAuth(credential)) = self.credentials.credential(provider)
        {
            let credential = if credential.is_expired() {
                self.refresh(provider, flow.as_ref(), cancel).await?
            } else {
                credential
            };
            return Ok(flow.request_auth(&credential));
        }
        let auth = self.resolve_auth(provider).ok_or_else(|| {
            let hint = match registered.api_key_env_vars() {
                [] => String::new(),
                vars => format!(". Set {}", vars.join(" or ")),
            };
            let sign_in = if registered.oauth().is_some() {
                " or sign in"
            } else {
                ""
            };
            format!("No API key for provider {provider}{hint}{sign_in}")
        })?;
        Ok(RequestAuth {
            api_key: auth.api_key,
            ..RequestAuth::default()
        })
    }

    /// Refreshes the sign-in of `provider` unless another request already did.
    async fn refresh(
        &self,
        provider: &str,
        flow: &dyn OAuthFlow,
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, String> {
        let lock = self.refresh_lock(provider);
        let _guard = lock.lock().await;
        let Some(Credential::OAuth(current)) = self.credentials.credential(provider) else {
            return Err(format!("{provider} is no longer signed in"));
        };
        if !current.is_expired() {
            return Ok(current);
        }
        let refreshed = flow
            .refresh(current, cancel.clone())
            .await
            .map_err(|error| {
                format!("Could not refresh the {provider} sign-in ({error}); sign in again")
            })?;
        if let Err(error) = self
            .credentials
            .store(provider, &Credential::OAuth(refreshed.clone()))
        {
            tracing::warn!(provider, %error, "could not save refreshed tokens");
        }
        Ok(refreshed)
    }

    /// Providers whose credentials resolve.
    pub fn available_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers()
            .into_iter()
            .filter(|p| self.resolve_auth(p.id()).is_some())
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
        if options.api_key.is_some() {
            return provider.stream(model, context, options);
        }
        // Resolve (and maybe refresh) credentials first, then stream.
        let models = self.clone();
        let mut model = model.clone();
        let context = context.clone();
        let skeleton = skeleton();
        let resolve = async move {
            match models.request_auth(&model.provider, &options.cancel).await {
                Ok(auth) => {
                    options.api_key = auth.api_key;
                    for (name, value) in auth.headers {
                        options.headers.entry(name).or_insert(value);
                    }
                    if let Some(base_url) = auth.base_url {
                        model.base_url = base_url;
                    }
                    provider.stream(&model, &context, options)
                }
                Err(_) if options.cancel.is_cancelled() => crate::event::aborted_stream(skeleton),
                Err(error) => error_stream(skeleton, error),
            }
        };
        Box::pin(futures::stream::once(resolve).flatten())
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
    fn clones_share_providers() {
        let models = Models::new().with_env(env(&[]));
        let clone = models.clone();
        models.register(Arc::new(FauxProvider::default()));
        assert_eq!(clone.providers().len(), 1);
        let model = clone.models(None)[0].clone();
        assert_eq!(clone.model_source(&model), ModelSource::BuiltIn);
        let mut listed = model.clone();
        listed.id = "listed-only".into();
        assert_eq!(clone.model_source(&listed), ModelSource::Upstream);
        assert!(clone.remove_provider(&model.provider));
        assert!(models.providers().is_empty());
        assert!(!models.remove_provider("nope"));
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
            models.provider("deepseek").unwrap().models().len()
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
            Some("No API key for provider anthropic. Set ANTHROPIC_API_KEY or sign in")
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
