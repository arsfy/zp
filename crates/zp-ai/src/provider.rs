//! Providers own a model catalog, auth requirements and the streaming implementation.

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::api;
use crate::event::{AssistantMessageStream, error_stream};
use crate::model::{Api, Model};
use crate::oauth::OAuthFlow;
use crate::options::StreamOptions;
use crate::sync::{SyncError, UpstreamModel, list_upstream_models};
use crate::types::{AssistantMessage, Context};

/// A source of models.
pub trait Provider: Send + Sync {
    /// Stable id, e.g. `anthropic`.
    fn id(&self) -> &str;

    /// Display name, e.g. `Anthropic`.
    fn name(&self) -> &str;

    /// The provider's own catalog. [`crate::Models`] layers models.dev metadata and the
    /// upstream model list on top of it; use `Models::models` for the effective list.
    fn models(&self) -> Arc<[Model]>;

    /// Environment variables that may hold the API key, in priority order.
    fn api_key_env_vars(&self) -> &[String] {
        &[]
    }

    /// Whether requests need a credential (an API key or a sign-in).
    fn requires_api_key(&self) -> bool {
        true
    }

    /// Whether an API key can be entered (`false` for sign-in-only providers).
    fn accepts_api_key(&self) -> bool {
        true
    }

    /// Signing in with the provider's account instead of an API key, if supported.
    fn oauth(&self) -> Option<Arc<dyn OAuthFlow>> {
        None
    }

    /// Model used when none is specified.
    fn default_model_id(&self) -> Option<&str> {
        None
    }

    /// Protocol and base URL of the provider's main endpoint. Used to list upstream models
    /// and as the default for models the catalog does not describe.
    fn api_endpoint(&self) -> Option<(Api, &str)> {
        None
    }

    /// The matching provider id on models.dev, if any.
    fn models_dev_id(&self) -> Option<&str> {
        None
    }

    /// Lists the models the provider currently offers to `api_key`. The default lists
    /// [`Self::api_endpoint`] with its protocol's list endpoint.
    fn list_models(
        &self,
        api_key: Option<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Vec<UpstreamModel>, SyncError>> {
        let endpoint = self
            .api_endpoint()
            .map(|(api, base_url)| (api, base_url.to_owned()));
        Box::pin(async move {
            let (api, base_url) = endpoint.ok_or(SyncError::Unsupported)?;
            list_upstream_models(api, &base_url, api_key.as_deref(), &cancel).await
        })
    }

    /// Streams a response. `options.api_key` has already been resolved.
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageStream;
}

/// A provider reached over HTTP with one of the built-in wire protocols.
pub struct HttpProvider {
    id: String,
    name: String,
    env_vars: Vec<String>,
    requires_api_key: bool,
    default_model: Option<String>,
    endpoint: Option<(Api, String)>,
    models_dev_id: Option<String>,
    models: Arc<[Model]>,
    oauth: Option<Arc<dyn OAuthFlow>>,
    accepts_api_key: bool,
}

impl HttpProvider {
    /// Creates a provider. Model `provider` fields are set to `id`.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        env_vars: impl IntoIterator<Item = impl Into<String>>,
        models: Vec<Model>,
    ) -> Self {
        let id = id.into();
        let models: Vec<Model> = models
            .into_iter()
            .map(|mut model| {
                model.provider.clone_from(&id);
                model
            })
            .collect();
        Self {
            id,
            name: name.into(),
            env_vars: env_vars.into_iter().map(Into::into).collect(),
            requires_api_key: true,
            default_model: None,
            endpoint: None,
            models_dev_id: None,
            models: models.into(),
            oauth: None,
            accepts_api_key: true,
        }
    }

    /// Lets users sign in with `flow` instead of entering an API key.
    pub fn with_oauth(mut self, flow: Arc<dyn OAuthFlow>) -> Self {
        self.oauth = Some(flow);
        self
    }

    /// Only signing in is possible (no API keys).
    pub fn oauth_only(mut self) -> Self {
        self.accepts_api_key = false;
        self
    }

    /// Sets the default model id.
    pub fn with_default_model(mut self, id: impl Into<String>) -> Self {
        self.default_model = Some(id.into());
        self
    }

    /// Sets the main endpoint (see [`Provider::api_endpoint`]).
    pub fn with_api_endpoint(mut self, api: Api, base_url: impl Into<String>) -> Self {
        self.endpoint = Some((api, base_url.into()));
        self
    }

    /// Links the provider to its models.dev entry.
    pub fn with_models_dev_id(mut self, id: impl Into<String>) -> Self {
        self.models_dev_id = Some(id.into());
        self
    }

    /// Marks the provider as keyless (e.g. a local server).
    pub fn without_api_key(mut self) -> Self {
        self.requires_api_key = false;
        self
    }
}

impl Provider for HttpProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn models(&self) -> Arc<[Model]> {
        self.models.clone()
    }

    fn api_key_env_vars(&self) -> &[String] {
        &self.env_vars
    }

    fn requires_api_key(&self) -> bool {
        self.requires_api_key
    }

    fn default_model_id(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    fn accepts_api_key(&self) -> bool {
        self.accepts_api_key
    }

    fn oauth(&self) -> Option<Arc<dyn OAuthFlow>> {
        self.oauth.clone()
    }

    fn api_endpoint(&self) -> Option<(Api, &str)> {
        self.endpoint
            .as_ref()
            .map(|(api, base_url)| (*api, base_url.as_str()))
    }

    fn models_dev_id(&self) -> Option<&str> {
        self.models_dev_id.as_deref()
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        match model.api {
            Api::AnthropicMessages => {
                api::anthropic::stream(model.clone(), context.clone(), options)
            }
            Api::OpenAiCompletions => {
                api::openai_completions::stream(model.clone(), context.clone(), options)
            }
            Api::OpenAiResponses | Api::OpenAiCodexResponses => {
                api::openai_responses::stream(model.clone(), context.clone(), options)
            }
            Api::Faux => error_stream(
                AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id),
                "the faux API is only available through FauxProvider",
            ),
        }
    }
}
