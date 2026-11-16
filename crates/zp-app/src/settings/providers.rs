//! The Providers page: the configured providers (keys in the keychain, model list
//! refreshes), and a dialog to add one — a built-in provider's API key or a custom
//! OpenAI-compatible endpoint.

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, Focusable as _, FontWeight, IntoElement,
    MouseButton, Window, div, prelude::*, px,
};
use gpui_elements::editable_text::EditableTextState;
use zp_ai::AuthSource;
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
    error: Option<String>,
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
            error: None,
        }
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
            models: text(&self.models)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }
}

/// The add / change provider dialog.
pub enum ProviderDialog {
    /// Pick a provider to add.
    Choose { filter: Entity<EditableTextState> },
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
                self.report(
                    Ok(format!(
                        "Saved the key of {provider}; refreshing its models"
                    )),
                    cx,
                );
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
        let result = credentials.remove(provider).map(|()| {
            AppModels::credential_changed(cx, provider);
            format!("Removed the key of {provider}")
        });
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

    /// Validates and saves the form: settings, registry, key, then a model list refresh.
    pub fn save_provider_form(&mut self, cx: &mut Context<Self>) {
        let Some(ProviderDialog::Custom(form)) = &self.dialog else {
            return;
        };
        let editing = form.editing.clone();
        let key = form.key.read(cx).as_str().trim().to_owned();
        let result = (|| -> anyhow::Result<String> {
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
            AppSettings::update(cx, |settings| {
                settings.providers.retain(|p| p.id != def.id);
                settings.providers.push(def.clone());
            })?;
            models.register(provider);
            if !key.is_empty() {
                cx.global::<AppModels>().credentials.set(&def.id, &key)?;
            }
            AppModels::credential_changed(cx, &def.id);
            Ok(format!("Saved provider {}", def.id))
        })();
        match result {
            Ok(message) => {
                self.close_dialog(cx);
                self.report(Ok(message), cx);
            }
            Err(error) => {
                if let Some(ProviderDialog::Custom(form)) = &mut self.dialog {
                    form.error = Some(format!("{error:#}"));
                }
                cx.notify();
            }
        }
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
                Ok(format!("Removed provider {id}"))
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
                                .child("Keys live in the system keychain; variables like OPENAI_API_KEY work too."),
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
                    .child("No provider is configured yet. Click Add Provider… to add an API key or a custom endpoint."),
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
            let needs_key = provider.requires_api_key() || is_custom;
            let actions =
                div()
                    .flex()
                    .gap_1p5()
                    .when(needs_key, |this| {
                        this.child(
                            button(
                                ("set-key", index),
                                if stored { "Replace Key" } else { "Set Key" },
                                ButtonKind::Normal,
                                cx,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.edit_key(&set_id, window, cx),
                            )),
                        )
                    })
                    .when(stored, |this| {
                        this.child(
                            button(("remove-key", index), "Remove Key", ButtonKind::Danger, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.remove_key(&remove_id, cx)
                                })),
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
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.refresh_provider(&refresh_id, cx),
                            )),
                        )
                    })
                    .when(is_custom, |this| {
                        this.child(
                            button(("edit-provider", index), "Edit", ButtonKind::Normal, cx)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_provider_form(Some(&edit_id), window, cx)
                                })),
                        )
                        .child(
                            button(("delete-provider", index), "Delete", ButtonKind::Danger, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_provider(&delete_id, cx)
                                })),
                        )
                    });

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
                        div()
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
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(models_text),
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
        let colors = cx.theme().colors.clone();
        let content = match dialog {
            ProviderDialog::Choose { filter } => self.render_choose(filter, cx),
            ProviderDialog::Key {
                provider,
                input,
                error,
            } => self.render_key_step(provider, input, error.clone(), cx),
            ProviderDialog::Custom(form) => self.render_provider_form(form, cx),
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
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.surface)
                        .shadow_lg()
                        .child(content),
                )
                .into_any_element(),
        )
    }

    fn dialog_title(title: String, subtitle: &str, cx: &App) -> impl IntoElement {
        let colors = &cx.theme().colors;
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(!subtitle.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
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
                "Choose a provider to add its API key, or add your own endpoint.",
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
                                    this.edit_key(&id, window, cx)
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
                .items_start()
                .gap_2()
                .child(label(name))
                .child(div().flex_1().child(field))
        };
        let anthropic = form.anthropic;
        let editing = form.editing.clone();
        div()
            .flex()
            .flex_col()
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
                super::text_field(&form.base_url, "http://localhost:11434/v1", cx).into_any_element(),
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
                super::text_block(&form.headers, "X-Team: platform", cx).into_any_element(),
            ))
            .child(row(
                "Models",
                super::text_block(&form.models, "One model id per line (empty: the endpoint's list)", cx)
                    .into_any_element(),
            ))
            .when_some(form.error.clone(), |this, error| {
                this.child(div().text_size(px(12.)).text_color(colors.error).child(error))
            })
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .justify_end()
                    .when(editing.is_none(), |this| {
                        this.child(
                            button("dialog-back", "Back", ButtonKind::Normal, cx)
                                .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx))),
                        )
                    })
                    .child(
                        button("dialog-cancel", "Cancel", ButtonKind::Normal, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .child(
                        button("save-provider", "Save Provider", ButtonKind::Primary, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.save_provider_form(cx))),
                    ),
            )
            .into_any_element()
    }
}
