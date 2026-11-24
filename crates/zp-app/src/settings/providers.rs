//! The Providers page: the configured providers (keys or sign-ins in the keychain, model
//! list refreshes), and a dialog to add one — signing in with an account, a built-in
//! provider's API key, or a custom OpenAI-compatible endpoint.

use futures::StreamExt as _;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Entity, Focusable as _, FontWeight,
    IntoElement, MouseButton, ScrollHandle, Task, Window, div, prelude::*, px,
};
use gpui_elements::editable_text::EditableTextState;
use zp_ai::oauth::{LoginEvent, LoginInteraction, LoginOptions, ManualInput};
use zp_ai::{AuthSource, CancellationToken, UpstreamModel};
use zp_core::model_catalog::SyncOutcome;
use zp_core::settings::CustomProvider;

use super::{ButtonKind, SettingsWindow, ago, button, input, page_body, secret_field, text_field};
use crate::store::{AppModels, AppSettings};
use crate::theme::ActiveTheme;

/// The add / edit form of a custom provider.
pub struct ProviderForm {
    /// The id being edited; `None` for a new provider.
    editing: Option<String>,
    pub id: Entity<EditableTextState>,
    pub name: Entity<EditableTextState>,
    pub base_url: Entity<EditableTextState>,
    pub key: Entity<EditableTextState>,
    pub headers: Entity<EditableTextState>,
    pub models: Entity<EditableTextState>,
    anthropic: bool,
    prompt_cache_key_mode: Option<zp_ai::PromptCacheKeyMode>,
    supports_long_cache_retention: Option<bool>,
    error: Option<String>,
    /// The last "Test" of the endpoint's model list.
    test: Option<EndpointTest>,
    _test_task: Option<Task<()>>,
    /// The dialog body's scroll position, to bring test results into view.
    scroll: ScrollHandle,
}

/// The result of listing the models of the form's endpoint.
#[derive(Clone, Debug)]
pub enum EndpointTest {
    /// The request is in flight.
    Running,
    /// The endpoint answered with these models.
    Listed(Vec<UpstreamModel>),
    /// The request failed.
    Failed(String),
}

impl ProviderForm {
    fn new(def: Option<&CustomProvider>, cx: &mut App) -> Self {
        let headers: Vec<String> = def
            .map(|d| d.headers.iter().map(|(k, v)| format!("{k}: {v}")).collect())
            .unwrap_or_default();
        Self {
            editing: def.map(|d| d.id.clone()),
            id: input(def.map(|d| d.id.clone()).unwrap_or_default(), cx),
            name: input(def.map(|d| d.name.clone()).unwrap_or_default(), cx),
            base_url: input(def.map(|d| d.base_url.clone()).unwrap_or_default(), cx),
            key: input("", cx),
            headers: input(headers.join("\n"), cx),
            models: input(def.map(|d| d.models.join("\n")).unwrap_or_default(), cx),
            anthropic: def.and_then(|d| d.api.as_deref()) == Some("anthropic-messages"),
            prompt_cache_key_mode: def.and_then(|d| d.prompt_cache_key_mode),
            supports_long_cache_retention: def.and_then(|d| d.supports_long_cache_retention),
            error: None,
            test: None,
            _test_task: None,
            scroll: ScrollHandle::new(),
        }
    }

    /// Index of the "Model list" row among the form's rows (title, [id], name, base URL,
    /// API, key, headers, models, model list), to scroll results into view.
    fn test_row(&self) -> usize {
        if self.editing.is_some() { 7 } else { 8 }
    }

    /// The last endpoint test (tests).
    #[cfg(test)]
    pub fn endpoint_test(&self) -> Option<&EndpointTest> {
        self.test.as_ref()
    }

    /// The error shown after a failed save (tests).
    #[cfg(test)]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the provider being edited already has a key (stored in the keychain).
    fn has_saved_key(&self, cx: &App) -> bool {
        self.editing.as_ref().is_some_and(|id| {
            cx.global::<AppModels>()
                .models
                .resolve_auth(id)
                .is_some_and(|auth| auth.api_key.is_some())
        })
    }

    /// Whether the form's base URL is the saved one of the provider being edited (ignoring
    /// surrounding spaces and a trailing `/`).
    fn base_url_unchanged(&self, cx: &App) -> bool {
        let normalize = |url: &str| url.trim().trim_end_matches('/').to_owned();
        let typed = normalize(self.base_url.read(cx).as_str());
        self.editing.as_ref().is_some_and(|id| {
            AppSettings::get(cx)
                .providers
                .iter()
                .any(|p| &p.id == id && normalize(&p.base_url) == typed)
        })
    }

    /// A warning about the headers: those that look like secrets end up in plain text in
    /// settings.json.
    pub fn secret_headers_warning(&self, cx: &App) -> Option<String> {
        let headers = self.headers.read(cx).as_str();
        let secret: Vec<String> = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(name, _)| name.trim())
            .filter(|name| zp_core::providers::looks_like_secret_header(name))
            .map(|name| format!("`{name}`"))
            .collect();
        (!secret.is_empty()).then(|| {
            format!(
                "{} look{} like a secret: headers are stored in plain text in settings.json; put secrets in the API key field.",
                secret.join(", "),
                if secret.len() == 1 { "s" } else { "" }
            )
        })
    }

    /// A warning about the base URL: a key typed or stored would be sent unencrypted.
    pub fn plain_http_warning(&self, cx: &App) -> Option<String> {
        let has_key = !self.key.read(cx).as_str().trim().is_empty() || self.has_saved_key(cx);
        zp_core::providers::plain_http_key_warning(self.base_url.read(cx).as_str(), has_key)
    }

    /// The provider described by the form.
    fn definition(&self, cx: &App) -> anyhow::Result<CustomProvider> {
        let text = |input: &Entity<EditableTextState>| input.read(cx).as_str().trim().to_owned();
        let mut headers = std::collections::BTreeMap::new();
        for line in text(&self.headers)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("headers are `Name: value` lines, got `{line}`"))?;
            headers.insert(name.trim().to_owned(), value.trim().to_owned());
        }
        Ok(CustomProvider {
            id: text(&self.id),
            name: text(&self.name),
            base_url: text(&self.base_url),
            api: self.anthropic.then(|| "anthropic-messages".to_owned()),
            headers,
            prompt_cache_key_mode: self.prompt_cache_key_mode,
            supports_long_cache_retention: self.supports_long_cache_retention,
            models: text(&self.models)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }
}

/// A running sign-in.
pub struct LoginDialog {
    provider: String,
    /// The page to open (and what to do there).
    url: Option<(String, String)>,
    /// The code to enter at a verification page.
    device: Option<(String, String)>,
    progress: Option<String>,
    /// A pasted redirect URL or code, for when the browser cannot reach ZP.
    code: Entity<EditableTextState>,
    manual: ManualInput,
    cancel: CancellationToken,
    error: Option<String>,
    _task: Task<()>,
}

/// The add / change provider dialog.
pub enum ProviderDialog {
    /// Pick a provider to add.
    Choose { filter: Entity<EditableTextState> },
    /// Sign in or enter an API key for `provider`.
    Method { provider: String },
    /// Signing in to a provider.
    Login(Box<LoginDialog>),
    /// Enter the API key of `provider`.
    Key {
        provider: String,
        input: Entity<EditableTextState>,
        error: Option<String>,
    },
    /// Add or edit a custom endpoint.
    Custom(Box<ProviderForm>),
}

/// How a provider's key resolves, for display.
fn key_status(source: Option<AuthSource>) -> (String, bool) {
    match source {
        Some(AuthSource::Stored) => ("Key stored in the keychain".into(), true),
        Some(AuthSource::OAuth) => ("Signed in".into(), true),
        Some(AuthSource::Env(var)) => (format!("Key from ${var}"), true),
        Some(AuthSource::NotRequired) => ("No key needed".into(), true),
        Some(AuthSource::Explicit) => ("Key set".into(), true),
        None => ("No key".into(), false),
    }
}

impl SettingsWindow {
    /// Opens the dialog to add a provider.
    pub fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter = input("", cx);
        filter.read(cx).focus_handle(cx).focus(window, cx);
        let refresh = cx.subscribe(
            &filter,
            |_, _, _: &gpui_elements::editable_text::TextChanged, cx| cx.notify(),
        );
        self.dialog_subscription = Some(refresh);
        self.dialog = Some(ProviderDialog::Choose { filter });
        cx.notify();
    }

    pub fn close_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(ProviderDialog::Login(login)) = &self.dialog {
            login.cancel.cancel();
        }
        self.dialog = None;
        self.dialog_subscription = None;
        cx.notify();
    }

    /// The open dialog (tests).
    #[cfg(test)]
    pub fn dialog(&self) -> Option<&ProviderDialog> {
        self.dialog.as_ref()
    }

    /// Providers offered by the dialog: id, name, configured.
    pub fn addable_providers(&self, cx: &App) -> Vec<(String, String, bool)> {
        let filter = match &self.dialog {
            Some(ProviderDialog::Choose { filter }) => {
                filter.read(cx).as_str().trim().to_lowercase()
            }
            _ => String::new(),
        };
        let custom: Vec<String> = AppSettings::get(cx)
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let models = &cx.global::<AppModels>().models;
        let mut providers: Vec<(String, String, bool)> = models
            .providers()
            .iter()
            .filter(|p| !custom.iter().any(|c| c == p.id()))
            .filter(|p| p.requires_api_key())
            .map(|p| {
                (
                    p.id().to_owned(),
                    p.name().to_owned(),
                    models.resolve_auth(p.id()).is_some(),
                )
            })
            .filter(|(id, name, _)| {
                filter.is_empty() || id.contains(&filter) || name.to_lowercase().contains(&filter)
            })
            .collect();
        // Not yet configured first.
        providers.sort_by_key(|(_, _, configured)| *configured);
        providers
    }

    /// Continues with `provider`: offers signing in and an API key when both work,
    /// otherwise goes to the one that does.
    pub fn choose_provider(&mut self, provider: &str, window: &mut Window, cx: &mut Context<Self>) {
        let registered = cx.global::<AppModels>().models.provider(provider);
        let (oauth, keys) = registered.as_ref().map_or((false, true), |p| {
            (p.oauth().is_some(), p.accepts_api_key())
        });
        match (oauth, keys) {
            (true, true) => {
                self.dialog_subscription = None;
                self.dialog = Some(ProviderDialog::Method {
                    provider: provider.to_owned(),
                });
                cx.notify();
            }
            (true, false) => self.start_login(provider, window, cx),
            _ => self.edit_key(provider, window, cx),
        }
    }

    /// Signs in to `provider`: the dialog shows the page to open (it is opened in the
    /// browser) or the code to enter, and accepts a pasted redirect URL.
    pub fn start_login(&mut self, provider: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ProviderDialog::Login(login)) = &self.dialog {
            login.cancel.cancel();
        }
        let models = cx.global::<AppModels>().models.clone();
        let options = LoginOptions {
            device_id: cx
                .try_global::<crate::store::AppPaths>()
                .and_then(|paths| paths.data_dir.clone())
                .and_then(|dir| zp_core::paths::device_id(&dir).ok()),
            agent_name: None,
        };
        let cancel = CancellationToken::new();
        let (events, mut received) = futures::channel::mpsc::unbounded();
        let (interaction, manual) = LoginInteraction::new(
            cancel.clone(),
            move |event| {
                let _ = events.unbounded_send(event);
            },
            options,
        );
        let id = provider.to_owned();
        let login =
            gpui_tokio::Tokio::spawn(cx, async move { models.login(&id, interaction).await });
        let id = provider.to_owned();
        let task = cx.spawn(async move |this, cx| {
            let pump = async {
                while let Some(event) = received.next().await {
                    if this
                        .update(cx, |this, cx| this.login_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            };
            // Events stop once the login is over (its sender is dropped).
            let (result, ()) = futures::join!(login, pump);
            let result = result.unwrap_or_else(|error| Err(error.to_string()));
            let _ = this.update(cx, |this, cx| this.login_finished(&id, result, cx));
        });
        let code = input("", cx);
        code.read(cx).focus_handle(cx).focus(window, cx);
        self.dialog_subscription = None;
        self.dialog = Some(ProviderDialog::Login(Box::new(LoginDialog {
            provider: provider.to_owned(),
            url: None,
            device: None,
            progress: Some("Starting…".into()),
            code,
            manual,
            cancel,
            error: None,
            _task: task,
        })));
        cx.notify();
    }

    fn login_event(&mut self, event: LoginEvent, cx: &mut Context<Self>) {
        let Some(ProviderDialog::Login(login)) = &mut self.dialog else {
            return;
        };
        match event {
            LoginEvent::AuthUrl { url, instructions } => {
                cx.open_url(&url);
                login.url = Some((url, instructions));
                login.progress = None;
            }
            LoginEvent::DeviceCode {
                user_code,
                verification_uri,
            } => {
                cx.write_to_clipboard(ClipboardItem::new_string(user_code.clone()));
                cx.open_url(&verification_uri);
                login.device = Some((user_code, verification_uri));
                login.progress = None;
            }
            LoginEvent::Progress(message) => login.progress = Some(message),
        }
        cx.notify();
    }

    fn login_finished(
        &mut self,
        provider: &str,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let current = matches!(&self.dialog, Some(ProviderDialog::Login(login)) if login.provider == provider);
        match result {
            Ok(()) => {
                AppModels::credential_changed(cx, provider);
                if current {
                    self.close_dialog(cx);
                }
                self.report(Ok(()), cx);
            }
            Err(error) => {
                if let Some(ProviderDialog::Login(login)) = &mut self.dialog
                    && current
                    && !login.cancel.is_cancelled()
                {
                    login.error = Some(error);
                    login.progress = None;
                    cx.notify();
                }
            }
        }
    }

    /// The running sign-in's page to open and error (tests).
    #[cfg(test)]
    pub fn login_state(&self) -> Option<(Option<String>, Option<String>)> {
        match &self.dialog {
            Some(ProviderDialog::Login(login)) => Some((
                login.url.as_ref().map(|(url, _)| url.clone()),
                login.error.clone(),
            )),
            _ => None,
        }
    }

    /// Hands the pasted redirect URL or code to the running sign-in.
    pub fn submit_login_code(&mut self, cx: &mut Context<Self>) {
        let Some(ProviderDialog::Login(login)) = &mut self.dialog else {
            return;
        };
        let text = login.code.read(cx).as_str().trim().to_owned();
        if text.is_empty() {
            return;
        }
        login.manual.submit(text);
        login.error = None;
        login.progress = Some("Checking the code…".into());
        cx.notify();
    }

    /// Signs out of `provider` (deletes the stored tokens).
    pub fn sign_out(&mut self, provider: &str, cx: &mut Context<Self>) {
        let credentials = cx.global::<AppModels>().credentials.clone();
        let result = credentials
            .remove(provider)
            .map(|()| AppModels::credential_changed(cx, provider));
        self.report(result, cx);
    }

    /// Starts entering a key for `provider` (in the dialog).
    pub fn edit_key(&mut self, provider: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = input("", cx);
        field.read(cx).focus_handle(cx).focus(window, cx);
        self.dialog_subscription = None;
        self.dialog = Some(ProviderDialog::Key {
            provider: provider.to_owned(),
            input: field,
            error: None,
        });
        cx.notify();
    }

    /// The key being entered (tests).
    #[cfg(any(test, feature = "visual-tests"))]
    pub fn key_input(&self) -> Option<Entity<EditableTextState>> {
        match &self.dialog {
            Some(ProviderDialog::Key { input, .. }) => Some(input.clone()),
            _ => None,
        }
    }

    /// Stores the entered key, then refreshes the provider's model list.
    pub fn save_key(&mut self, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(ProviderDialog::Login(_))) {
            return self.submit_login_code(cx);
        }
        let Some(ProviderDialog::Key {
            provider, input, ..
        }) = &self.dialog
        else {
            return;
        };
        let provider = provider.clone();
        let key = input.read(cx).as_str().trim().to_owned();
        let credentials = cx.global::<AppModels>().credentials.clone();
        match credentials.set(&provider, &key) {
            Ok(()) => {
                AppModels::credential_changed(cx, &provider);
                self.close_dialog(cx);
                self.report(Ok(()), cx);
            }
            Err(error) => {
                if let Some(ProviderDialog::Key { error: slot, .. }) = &mut self.dialog {
                    *slot = Some(format!("{error:#}"));
                }
                cx.notify();
            }
        }
    }

    /// Deletes the stored key of `provider`.
    pub fn remove_key(&mut self, provider: &str, cx: &mut Context<Self>) {
        let credentials = cx.global::<AppModels>().credentials.clone();
        let result = credentials
            .remove(provider)
            .map(|()| AppModels::credential_changed(cx, provider));
        self.report(result, cx);
    }

    pub fn refresh_provider(&mut self, provider: &str, cx: &mut Context<Self>) {
        AppModels::sync(cx, vec![provider.to_owned()], true);
        self.notice = Some((format!("Refreshing the models of {provider}…"), false));
        cx.notify();
    }

    /// Opens the custom provider form: new, or editing custom provider `id`.
    pub fn open_provider_form(
        &mut self,
        id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let def = id.and_then(|id| {
            AppSettings::get(cx)
                .providers
                .iter()
                .find(|p| p.id == id)
                .cloned()
        });
        let form = ProviderForm::new(def.as_ref(), cx);
        let focus = if def.is_some() { &form.name } else { &form.id };
        focus.read(cx).focus_handle(cx).focus(window, cx);
        self.dialog_subscription = None;
        self.dialog = Some(ProviderDialog::Custom(Box::new(form)));
        cx.notify();
    }

    /// The open provider form (tests).
    #[cfg(test)]
    pub fn provider_form(&self) -> Option<&ProviderForm> {
        match &self.dialog {
            Some(ProviderDialog::Custom(form)) => Some(form),
            _ => None,
        }
    }

    /// Validates and saves the form: key, settings, registry, then a model list refresh.
    /// The key comes first: the keychain is what fails most (access denied), and until the
    /// settings and registry change, saving again is a plain retry rather than "already
    /// exists".
    pub fn save_provider_form(&mut self, cx: &mut Context<Self>) {
        let Some(ProviderDialog::Custom(form)) = &self.dialog else {
            return;
        };
        let editing = form.editing.clone();
        let key = form.key.read(cx).as_str().trim().to_owned();
        let result = (|| -> anyhow::Result<()> {
            let mut def = form.definition(cx)?;
            if let Some(id) = &editing {
                def.id.clone_from(id);
            }
            let models = cx.global::<AppModels>().models.clone();
            let taken: Vec<String> = models
                .providers()
                .iter()
                .map(|p| p.id().to_owned())
                .filter(|id| Some(id) != editing.as_ref())
                .collect();
            zp_core::providers::validate_custom_provider(&def, &taken)?;
            let provider = zp_core::providers::custom_provider(&def)?;
            if !key.is_empty() {
                cx.global::<AppModels>().credentials.set(&def.id, &key)?;
            }
            AppSettings::update(cx, |settings| {
                settings.providers.retain(|p| p.id != def.id);
                settings.providers.push(def.clone());
            })?;
            models.register(provider);
            AppModels::credential_changed(cx, &def.id);
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.close_dialog(cx);
                self.report(Ok(()), cx);
            }
            Err(error) => {
                if let Some(ProviderDialog::Custom(form)) = &mut self.dialog {
                    form.error = Some(format!("{error:#}"));
                }
                cx.notify();
            }
        }
    }

    /// Lists the models of the form's endpoint the way a model list refresh would, with the
    /// key typed in the form (or, when editing with the saved base URL, the stored one),
    /// without saving anything.
    pub fn test_provider_form(&mut self, cx: &mut Context<Self>) {
        let Some(ProviderDialog::Custom(form)) = &mut self.dialog else {
            return;
        };
        let editing = form.editing.clone();
        let provider = form.definition(cx).and_then(|mut def| {
            if let Some(id) = &editing {
                def.id.clone_from(id);
            }
            if def.id.is_empty() {
                "test".clone_into(&mut def.id);
            }
            zp_core::providers::validate_custom_provider(&def, &[])?;
            zp_core::providers::custom_provider(&def)
        });
        let provider = match provider {
            Ok(provider) => provider,
            Err(error) => {
                form.test = Some(EndpointTest::Failed(format!("{error:#}")));
                form._test_task = None;
                cx.notify();
                return;
            }
        };
        let typed_key = form.key.read(cx).as_str().trim().to_owned();
        // The stored key only goes to the endpoint it was saved for.
        if typed_key.is_empty() && form.has_saved_key(cx) && !form.base_url_unchanged(cx) {
            form.test = Some(EndpointTest::Failed(
                "Enter the API key to test a changed base URL".into(),
            ));
            form._test_task = None;
            cx.notify();
            return;
        }
        let models = cx.global::<AppModels>().models.clone();
        let listing = gpui_tokio::Tokio::spawn(cx, async move {
            let cancel = CancellationToken::new();
            let api_key = if !typed_key.is_empty() {
                Some(typed_key)
            } else if let Some(id) = editing.filter(|id| models.resolve_auth(id).is_some()) {
                models
                    .request_auth(&id, &cancel)
                    .await
                    .map_err(zp_ai::SyncError::Failed)?
                    .api_key
            } else {
                None
            };
            provider.list_models(api_key, cancel).await
        });
        form.test = Some(EndpointTest::Running);
        // Replacing (or closing) the form drops the task, which aborts the request.
        form._test_task = Some(cx.spawn(async move |this, cx| {
            let result = match listing.await {
                Ok(Ok(models)) if models.is_empty() => {
                    EndpointTest::Failed("the endpoint listed no models".into())
                }
                Ok(Ok(models)) => EndpointTest::Listed(models),
                Ok(Err(error)) => EndpointTest::Failed(error.to_string()),
                Err(error) => EndpointTest::Failed(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(ProviderDialog::Custom(form)) = &mut this.dialog {
                    form.test = Some(result);
                    form.scroll.scroll_to_top_of_item(form.test_row());
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Shows `result` as the form's endpoint test (screenshots).
    #[cfg(feature = "visual-tests")]
    pub(crate) fn show_endpoint_test(&mut self, result: EndpointTest, cx: &mut Context<Self>) {
        if let Some(ProviderDialog::Custom(form)) = &mut self.dialog {
            form.test = Some(result);
            form.scroll.scroll_to_top_of_item(form.test_row());
        }
        cx.notify();
    }

    /// Removes custom provider `id` (with its key and cached model list).
    pub fn delete_provider(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |settings| settings.providers.retain(|p| p.id != id))
            .and_then(|()| {
                let app = cx.global::<AppModels>();
                app.models.remove_provider(id);
                app.credentials.remove(id)?;
                if let Some(cache) = AppModels::catalog_cache(cx) {
                    cache.clear_upstream(&cx.global::<AppModels>().models, id)?;
                }
                Ok(())
            });
        self.report(result, cx);
    }

    /// Ids of the providers listed on the page: configured ones and custom ones.
    pub fn configured_providers(&self, cx: &App) -> Vec<String> {
        let custom: Vec<String> = AppSettings::get(cx)
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let models = &cx.global::<AppModels>().models;
        models
            .providers()
            .iter()
            .filter(|p| custom.iter().any(|c| c == p.id()) || models.resolve_auth(p.id()).is_some())
            .map(|p| p.id().to_owned())
            .collect()
    }

    pub(super) fn render_providers(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let app = cx.global::<AppModels>();
        let models = app.models.clone();
        let credentials = app.credentials.clone();
        let running = app.sync.running;
        let last_outcomes: Vec<(String, SyncOutcome)> = app
            .sync
            .last
            .as_ref()
            .map(|(_, report)| report.upstream.clone())
            .unwrap_or_default();
        let status = AppModels::catalog_cache(cx).map(|cache| cache.status(&models));
        let custom: Vec<String> = AppSettings::get(cx)
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let configured = self.configured_providers(cx);

        let mut body = page_body().child(
            div()
                .pt_3()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Configured providers"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(colors.text_muted)
                                .child("Sign in with your account, or use an API key. Both live in the system keychain; variables like OPENAI_API_KEY work too."),
                        ),
                )
                .child(
                    button("add-provider", "Add Provider…", ButtonKind::Primary, cx).on_click(
                        cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx)),
                    ),
                ),
        );
        if configured.is_empty() {
            body = body.child(
                div()
                    .p_4()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(colors.border)
                    .text_color(colors.text_muted)
                    .child("No provider is configured yet. Click Add Provider… to sign in, add an API key or a custom endpoint."),
            );
        }
        for (index, id) in configured.iter().enumerate() {
            let Some(provider) = models.provider(id) else {
                continue;
            };
            let id = id.clone();
            let (key_text, has_key) = key_status(models.resolve_auth(&id).map(|a| a.source));
            let stored = credentials.is_stored(&id);
            let is_custom = custom.contains(&id);
            let model_count = models.models(Some(&id)).len();
            let listed = status
                .as_ref()
                .and_then(|s| s.upstream.iter().find(|(p, _, _)| *p == id))
                .map(|(_, at, count)| format!("list of {count} updated {}", ago(*at)));
            let failure = last_outcomes.iter().find_map(|(p, outcome)| match outcome {
                SyncOutcome::Failed(error) if *p == id => Some(error.clone()),
                _ => None,
            });
            let models_text = match listed {
                Some(listed) => format!("{model_count} models · {listed}"),
                None => format!("{model_count} models"),
            };

            let (set_id, remove_id, refresh_id, edit_id, delete_id) =
                (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
            let (login_id, logout_id) = (id.clone(), id.clone());
            let signed_in = models.is_signed_in(&id);
            let can_sign_in = provider.oauth().is_some();
            let needs_key =
                (provider.requires_api_key() && provider.accepts_api_key()) || is_custom;
            let actions = div()
                .flex()
                .gap_1p5()
                .when(can_sign_in, |this| {
                    this.child(
                        button(
                            ("sign-in", index),
                            if signed_in {
                                "Sign In Again"
                            } else {
                                "Sign In"
                            },
                            ButtonKind::Normal,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.start_login(&login_id, window, cx),
                        )),
                    )
                })
                .when(signed_in, |this| {
                    this.child(
                        button(("sign-out", index), "Sign Out", ButtonKind::Danger, cx).on_click(
                            cx.listener(move |this, _, _, cx| this.sign_out(&logout_id, cx)),
                        ),
                    )
                })
                .when(needs_key, |this| {
                    this.child(
                        button(
                            ("set-key", index),
                            if stored && !signed_in {
                                "Replace Key"
                            } else {
                                "Set Key"
                            },
                            ButtonKind::Normal,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.edit_key(&set_id, window, cx),
                        )),
                    )
                })
                .when(stored && !signed_in, |this| {
                    this.child(
                        button(("remove-key", index), "Remove Key", ButtonKind::Danger, cx)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_key(&remove_id, cx)),
                            ),
                    )
                })
                .when(provider.api_endpoint().is_some() && has_key, |this| {
                    this.child(
                        button(
                            ("refresh-models", index),
                            if running {
                                "Refreshing…"
                            } else {
                                "Refresh Models"
                            },
                            ButtonKind::Normal,
                            cx,
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.refresh_provider(&refresh_id, cx)
                            }),
                        ),
                    )
                })
                .when(is_custom, |this| {
                    this.child(
                        button(("edit-provider", index), "Edit", ButtonKind::Normal, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.open_provider_form(Some(&edit_id), window, cx)
                            }),
                        ),
                    )
                    .child(
                        button(("delete-provider", index), "Delete", ButtonKind::Danger, cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_provider(&delete_id, cx)
                            })),
                    )
                });

            let heading = div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(provider.name().to_owned()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .child(id.clone()),
                )
                .when(is_custom, |this| {
                    this.child(
                        div()
                            .self_center()
                            .px_1()
                            .rounded(px(3.))
                            .text_size(px(10.))
                            .bg(colors.code_background)
                            .text_color(colors.text_muted)
                            .child("custom"),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(if has_key {
                            colors.success
                        } else {
                            colors.text_muted
                        })
                        .child(key_text),
                );

            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .p_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(colors.border)
                    .child(
                        div().flex().flex_col().gap_0p5().child(heading).child(
                            div()
                                .text_size(px(12.))
                                .text_color(colors.text_muted)
                                .child(models_text),
                        ),
                    )
                    .when_some(failure, |this, error| {
                        this.child(
                            div()
                                .text_size(px(12.))
                                .text_color(colors.error)
                                .child(format!("Model list refresh failed: {error}")),
                        )
                    })
                    .child(actions),
            );
        }
        body.into_any_element()
    }

    /// The dialog over the settings window, if open.
    pub(super) fn render_provider_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let font_size = theme.metrics.font_size;
        let content = match dialog {
            ProviderDialog::Choose { filter } => self.render_choose(filter, cx),
            ProviderDialog::Key {
                provider,
                input,
                error,
            } => self.render_key_step(provider, input, error.clone(), cx),
            ProviderDialog::Custom(form) => self.render_provider_form(form, cx),
            ProviderDialog::Method { provider } => self.render_method(provider, cx),
            ProviderDialog::Login(login) => self.render_login(login, cx),
        };
        let footer = match dialog {
            ProviderDialog::Custom(form) => Some(self.render_provider_form_footer(form, cx)),
            _ => None,
        };
        // Forms scroll their own fields (to bring rows into view); the rest scroll here.
        let body = if footer.is_some() {
            content
        } else {
            div()
                .id("provider-dialog-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(content)
                .into_any_element()
        };
        Some(
            div()
                .id("provider-dialog-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(72.))
                .bg(gpui::rgba(0x0000_0088))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_dialog(cx)),
                )
                .child(
                    div()
                        .id("provider-dialog")
                        .key_context("ProviderDialog")
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .w(px(560.))
                        .max_h(px(560.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.surface)
                        .shadow_lg()
                        .text_size(font_size)
                        // Fields scroll; a form's buttons stay in view.
                        .child(body)
                        .children(footer),
                )
                .into_any_element(),
        )
    }

    fn dialog_title(title: String, subtitle: &str, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .text_size(theme.metrics.dialog_title_size())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(!subtitle.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(theme.metrics.dialog_detail_size())
                        .text_color(theme.colors.text_muted)
                        .child(subtitle.to_owned()),
                )
            })
    }

    fn render_choose(
        &self,
        filter: &Entity<EditableTextState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let providers = self.addable_providers(cx);
        let row = |id: gpui::ElementId, name: String, detail: String| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(px(6.))
                .cursor_pointer()
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(div().flex_1().child(name))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .child(detail),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(Self::dialog_title(
                "Add a provider".into(),
                "Choose a provider to sign in or add its API key, or add your own endpoint.",
                cx,
            ))
            .child(text_field(filter, "Search providers…", cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .children(providers.into_iter().enumerate().map(
                        |(index, (id, name, configured))| {
                            let detail = if configured {
                                "configured".to_owned()
                            } else {
                                id.clone()
                            };
                            row(("choose-provider", index).into(), name, detail).on_click(
                                cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.choose_provider(&id, window, cx)
                                }),
                            )
                        },
                    ))
                    .child(div().my_1().h(px(1.)).bg(colors.border))
                    .child(
                        row(
                            "choose-custom".into(),
                            "Custom endpoint…".into(),
                            "OpenAI-compatible or Anthropic API".into(),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_provider_form(None, window, cx)
                        })),
                    ),
            )
            .child(
                div().flex().justify_end().child(
                    button("dialog-cancel", "Cancel", ButtonKind::Normal, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                ),
            )
            .into_any_element()
    }

    fn render_key_step(
        &self,
        provider: &str,
        field: &Entity<EditableTextState>,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let registered = cx.global::<AppModels>().models.provider(provider);
        let name = registered
            .as_ref()
            .map_or(provider.to_owned(), |p| p.name().to_owned());
        let vars = registered
            .as_ref()
            .map(|p| p.api_key_env_vars().to_vec())
            .unwrap_or_default();
        let hint = if vars.is_empty() {
            "Stored in the system keychain.".to_owned()
        } else {
            format!(
                "Stored in the system keychain. ${} works too.",
                vars.join(" or $")
            )
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(Self::dialog_title(format!("{name} API key"), &hint, cx))
            .child(div().key_context("ApiKeyInput").child(secret_field(
                field,
                "Paste the API key",
                cx,
            )))
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.error)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .justify_end()
                    .child(
                        button("dialog-back", "Back", ButtonKind::Normal, cx).on_click(
                            cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx)),
                        ),
                    )
                    .child(
                        button("dialog-cancel", "Cancel", ButtonKind::Normal, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .child(
                        button("save-key", "Save", ButtonKind::Primary, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_method(&self, provider: &str, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let registered = cx.global::<AppModels>().models.provider(provider);
        let name = registered
            .as_ref()
            .map_or(provider.to_owned(), |p| p.name().to_owned());
        let flow = registered.as_ref().and_then(|p| p.oauth());
        let option = |id: &'static str, title: String, detail: String| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_2()
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .cursor_pointer()
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .child(detail),
                )
        };
        let (sign_in, id) = (provider.to_owned(), provider.to_owned());
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(Self::dialog_title(
                format!("Connect {name}"),
                "Sign in with your account, or paste an API key.",
                cx,
            ))
            .when_some(flow, |this, flow| {
                this.child(
                    option(
                        "method-sign-in",
                        flow.login_label().to_owned(),
                        format!("Use your {} in your browser.", flow.name()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_login(&sign_in, window, cx)
                    })),
                )
            })
            .child(
                option(
                    "method-api-key",
                    "Use an API key".into(),
                    "Paste a key from the provider's console.".into(),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.edit_key(&id, window, cx))),
            )
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .justify_end()
                    .child(
                        button("dialog-back", "Back", ButtonKind::Normal, cx).on_click(
                            cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx)),
                        ),
                    )
                    .child(
                        button("dialog-cancel", "Cancel", ButtonKind::Normal, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_login(&self, login: &LoginDialog, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let mono = cx.theme().metrics.mono_font.clone();
        let registered = cx.global::<AppModels>().models.provider(&login.provider);
        let name = registered
            .as_ref()
            .map_or(login.provider.clone(), |p| p.name().to_owned());
        let title = registered.as_ref().and_then(|p| p.oauth()).map_or_else(
            || format!("Sign in to {name}"),
            |f| f.login_label().to_owned(),
        );
        let muted = |text: String| {
            div()
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .whitespace_normal()
                .child(text)
        };
        let mut body = div().flex().flex_col().gap_2().child(Self::dialog_title(
            title,
            &format!("Connecting {name} to ZP."),
            cx,
        ));
        if let Some((code, uri)) = &login.device {
            let (copy, open) = (code.clone(), uri.clone());
            body = body
                .child(muted(format!(
                    "Enter this code at {uri} (it is copied to the clipboard):"
                )))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded(px(5.))
                                .bg(colors.code_background)
                                .font_family(mono.clone())
                                .text_size(px(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(code.clone()),
                        )
                        .child(
                            button("login-copy", "Copy", ButtonKind::Normal, cx).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                },
                            ),
                        )
                        .child(
                            button("login-open", "Open Page", ButtonKind::Normal, cx)
                                .on_click(move |_, _, cx| cx.open_url(&open)),
                        ),
                );
        }
        if let Some((url, instructions)) = &login.url {
            let open = url.clone();
            body = body
                .child(muted(instructions.clone()))
                .child(
                    div().flex().gap_1p5().child(
                        button("login-open", "Open Browser Again", ButtonKind::Normal, cx)
                            .on_click(move |_, _, cx| cx.open_url(&open)),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .gap_1p5()
                        .items_center()
                        .child(div().flex_1().key_context("ApiKeyInput").child(text_field(
                            &login.code,
                            "Paste the redirect URL or code",
                            cx,
                        )))
                        .child(
                            button("login-submit", "Submit", ButtonKind::Normal, cx)
                                .on_click(cx.listener(|this, _, _, cx| this.submit_login_code(cx))),
                        ),
                );
        }
        if let Some(progress) = &login.progress {
            body = body.child(muted(progress.clone()));
        }
        let failed = login.error.is_some();
        let retry = login.provider.clone();
        body.when_some(login.error.clone(), |this, error| {
            this.child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.error)
                    .whitespace_normal()
                    .child(error),
            )
        })
        .child(
            div()
                .flex()
                .gap_1p5()
                .justify_end()
                .when(failed, |this| {
                    this.child(
                        button("login-retry", "Try Again", ButtonKind::Primary, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.start_login(&retry, window, cx)
                            }),
                        ),
                    )
                })
                .child(
                    button("dialog-cancel", "Cancel", ButtonKind::Normal, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                ),
        )
        .into_any_element()
    }

    /// The "Test" button and what the endpoint listed.
    fn render_endpoint_test(&self, form: &ProviderForm, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let running = matches!(form.test, Some(EndpointTest::Running));
        let typed_models = !form.models.read(cx).as_str().trim().is_empty();
        let status = match &form.test {
            None => (
                "Fetch the endpoint's model list with these settings.".to_owned(),
                colors.text_muted,
            ),
            Some(EndpointTest::Running) => {
                ("Fetching the model list…".to_owned(), colors.text_muted)
            }
            Some(EndpointTest::Listed(models)) => (
                format!(
                    "{} model{} listed{}",
                    models.len(),
                    if models.len() == 1 { "" } else { "s" },
                    if typed_models {
                        "; the Models field above is used instead"
                    } else {
                        ""
                    },
                ),
                colors.success,
            ),
            Some(EndpointTest::Failed(error)) => (error.clone(), colors.error),
        };
        let listed = match &form.test {
            Some(EndpointTest::Listed(models)) => Some(models.clone()),
            _ => None,
        };
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        button(
                            "test-provider",
                            if running { "Testing…" } else { "Test" },
                            ButtonKind::Normal,
                            cx,
                        )
                        .debug_selector(|| "test-provider".into())
                        .flex_shrink_0()
                        .when(running, |this| this.opacity(0.6))
                        .when(!running, |this| {
                            this.on_click(cx.listener(|this, _, _, cx| this.test_provider_form(cx)))
                        }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(status.1)
                            .child(status.0),
                    ),
            )
            .when_some(listed, |this, models| {
                this.child(
                    // Part of the form's single scroll area: a nested scroller would take
                    // the wheel from the form halfway through.
                    div()
                        .debug_selector(|| "provider-test-models".into())
                        .rounded(px(4.))
                        .border_1()
                        .border_color(colors.border)
                        .py_1()
                        .children(models.into_iter().map(|model| {
                            let mut details = Vec::new();
                            if let Some(context) = model.context_window {
                                details
                                    .push(format!("{} context", crate::ui::format_tokens(context)));
                            }
                            if let Some(endpoints) = &model.endpoints {
                                let routes: Vec<&str> = endpoints
                                    .iter()
                                    .map(|e| match e.trim_start_matches("/v1") {
                                        "/chat/completions" => "chat",
                                        other => other.trim_start_matches('/'),
                                    })
                                    .collect();
                                details.push(routes.join(", "));
                            }
                            if !model.looks_like_chat() {
                                details.push("not a chat model".to_owned());
                            }
                            div()
                                .flex()
                                .gap_2()
                                .px_2()
                                .text_size(px(12.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(cx.theme().metrics.mono_font.clone())
                                        .child(model.id),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(colors.text_muted)
                                        .child(details.join(" · ")),
                                )
                        })),
                )
            })
            .into_any_element()
    }

    fn render_provider_form(&self, form: &ProviderForm, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let label = |text: &'static str| {
            div()
                .w(px(96.))
                .flex_shrink_0()
                .pt_1()
                .text_size(px(12.))
                .text_color(colors.text_muted)
                .child(text)
        };
        let row = |name: &'static str, field: AnyElement| {
            div()
                .flex()
                .w_full()
                .min_w_0()
                .items_start()
                .gap_2()
                .child(label(name))
                .child(div().flex_1().min_w_0().overflow_x_hidden().child(field))
        };
        // A field with a line of text below it.
        let noted = |field: AnyElement, note: Option<(String, gpui::Rgba, &'static str)>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(field)
                .when_some(note, |this, (text, color, selector)| {
                    this.child(
                        div()
                            .debug_selector(move || selector.into())
                            .text_size(px(12.))
                            .text_color(color)
                            .whitespace_normal()
                            .child(text),
                    )
                })
                .into_any_element()
        };
        let http_warning = form
            .plain_http_warning(cx)
            .map(|text| (text, colors.warning, "provider-http-warning"));
        let headers_note = Some(form.secret_headers_warning(cx).map_or_else(
            || {
                (
                    "Stored in settings.json; put secrets in the API key field.".to_owned(),
                    colors.text_muted,
                    "provider-headers-hint",
                )
            },
            |text| (text, colors.warning, "provider-headers-warning"),
        ));
        let anthropic = form.anthropic;
        let editing = form.editing.clone();
        div()
            .id("provider-dialog-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&form.scroll)
            .gap_2()
            .child(Self::dialog_title(
                match &editing {
                    Some(id) => format!("Edit {id}"),
                    None => "Custom endpoint".to_owned(),
                },
                "Any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, a company gateway…). Leave the models empty to use the ones the endpoint lists.",
                cx,
            ))
            .when(editing.is_none(), |this| {
                this.child(row("Id", super::text_field(&form.id, "e.g. ollama", cx).into_any_element()))
            })
            .child(row("Name", super::text_field(&form.name, "Shown in menus", cx).into_any_element()))
            .child(row(
                "Base URL",
                noted(
                    super::text_field(&form.base_url, "http://localhost:11434/v1", cx).into_any_element(),
                    http_warning,
                ),
            ))
            .child(row(
                "API",
                div()
                    .flex()
                    .child(super::segmented(
                        "provider-api",
                        [
                            ("OpenAI-compatible".to_owned(), !anthropic),
                            ("Anthropic Messages".to_owned(), anthropic),
                        ]
                        .into_iter(),
                        cx,
                        |this, index, cx| {
                            if let Some(ProviderDialog::Custom(form)) = &mut this.dialog {
                                form.anthropic = index == 1;
                            }
                            cx.notify();
                        },
                    ))
                    .into_any_element(),
            ))
            .child(row(
                "API key",
                secret_field(&form.key, "Optional; stored in the keychain", cx).into_any_element(),
            ))
            .child(row(
                "Headers",
                noted(
                    super::text_block(&form.headers, "X-Team: platform", cx).into_any_element(),
                    headers_note,
                ),
            ))
            .child(row(
                "Models",
                super::text_block(&form.models, "One model id per line (empty: the endpoint's list)", cx)
                    .into_any_element(),
            ))
            .child(row("Model list", self.render_endpoint_test(form, cx)))
            .when_some(form.error.clone(), |this, error| {
                this.child(div().text_size(px(12.)).text_color(colors.error).child(error))
            })
            .into_any_element()
    }

    /// The form's buttons, kept below the scrolling fields.
    fn render_provider_form_footer(
        &self,
        form: &ProviderForm,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_shrink_0()
            .gap_1p5()
            .justify_end()
            .when(form.editing.is_none(), |this| {
                this.child(
                    button("dialog-back", "Back", ButtonKind::Normal, cx).on_click(
                        cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx)),
                    ),
                )
            })
            .child(
                button("dialog-cancel", "Cancel", ButtonKind::Normal, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
            )
            .child(
                button("save-provider", "Save Provider", ButtonKind::Primary, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.save_provider_form(cx))),
            )
            .into_any_element()
    }
}
