//! The Providers page: API keys (in the keychain), model list refreshes and custom
//! OpenAI-compatible providers.

use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, FontWeight, IntoElement, Window, div,
    prelude::*, px,
};
use gpui_elements::editable_text::EditableTextState;
use zp_ai::AuthSource;
use zp_core::model_catalog::SyncOutcome;
use zp_core::settings::CustomProvider;

use super::{ButtonKind, SettingsWindow, ago, button, input, page_body, secret_field, section};
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
    /// Starts entering a key for `provider`.
    pub fn edit_key(&mut self, provider: &str, window: &mut Window, cx: &mut Context<Self>) {
        let field = input("", cx);
        field.read(cx).focus_handle(cx).focus(window, cx);
        self.key_editor = Some((provider.to_owned(), field));
        cx.notify();
    }

    /// The key being entered (tests).
    #[cfg(any(test, feature = "visual-tests"))]
    pub fn key_input(&self) -> Option<Entity<EditableTextState>> {
        self.key_editor.as_ref().map(|(_, input)| input.clone())
    }

    /// Stores the entered key, then refreshes the provider's model list.
    pub fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some((provider, field)) = self.key_editor.take() else {
            return;
        };
        let key = field.read(cx).as_str().trim().to_owned();
        let credentials = cx.global::<AppModels>().credentials.clone();
        let result = credentials.set(&provider, &key).map(|()| {
            AppModels::credential_changed(cx, &provider);
            format!("Saved the key of {provider}; refreshing its models")
        });
        self.report(result, cx);
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

    /// Opens the form for a new provider, or to edit custom provider `id`.
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
        self.provider_form = Some(form);
        cx.notify();
    }

    /// The open provider form (tests).
    #[cfg(test)]
    pub fn provider_form(&self) -> Option<&ProviderForm> {
        self.provider_form.as_ref()
    }

    /// Validates and saves the form: settings, registry, key, then a model list refresh.
    pub fn save_provider_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.provider_form else {
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
                self.provider_form = None;
                self.report(Ok(message), cx);
            }
            Err(error) => {
                if let Some(form) = &mut self.provider_form {
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

        let mut body = page_body().child(section(
            "Custom providers",
            "Any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, a company gateway…). Leave the models empty to use the ones the endpoint lists.",
            cx,
        ));
        body = match &self.provider_form {
            None => body.child(div().flex().child(
                button("add-provider", "Add Provider…", ButtonKind::Normal, cx).on_click(
                    cx.listener(|this, _, window, cx| this.open_provider_form(None, window, cx)),
                ),
            )),
            Some(form) => body.child(self.render_provider_form(form, cx)),
        };
        body = body.child(section(
            "API keys",
            "Keys are stored in the system keychain. Environment variables such as OPENAI_API_KEY are used when no key is stored.",
            cx,
        ));

        for (index, provider) in models.providers().iter().enumerate() {
            let id = provider.id().to_owned();
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
            let editing = self
                .key_editor
                .as_ref()
                .filter(|(provider, _)| *provider == id)
                .map(|(_, field)| field.clone());

            let (set_id, remove_id, refresh_id, edit_id, delete_id) =
                (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
            let actions =
                div()
                    .flex()
                    .gap_1p5()
                    .child(
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
                    .child(match editing {
                        Some(field) => div()
                            .key_context("ApiKeyInput")
                            .flex()
                            .gap_1p5()
                            .items_center()
                            .child(div().flex_1().child(secret_field(
                                &field,
                                "Paste the API key",
                                cx,
                            )))
                            .child(
                                button(("save-key", index), "Save", ButtonKind::Primary, cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.save_key(cx))),
                            )
                            .child(
                                button(("cancel-key", index), "Cancel", ButtonKind::Normal, cx)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.key_editor = None;
                                        cx.notify();
                                    })),
                            )
                            .into_any_element(),
                        None => actions.into_any_element(),
                    }),
            );
        }

        body.into_any_element()
    }

    fn render_provider_form(&self, form: &ProviderForm, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let label = |text: &'static str| {
            div()
                .w(px(110.))
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
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded(px(8.))
            .border_1()
            .border_color(colors.accent)
            .when(form.editing.is_none(), |this| {
                this.child(row(
                    "Id",
                    super::text_field(&form.id, "e.g. ollama", cx).into_any_element(),
                ))
            })
            .child(row(
                "Name",
                super::text_field(&form.name, "Shown in menus", cx).into_any_element(),
            ))
            .child(row(
                "Base URL",
                super::text_field(&form.base_url, "http://localhost:11434/v1", cx)
                    .into_any_element(),
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
                            if let Some(form) = &mut this.provider_form {
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
                super::text_block(
                    &form.models,
                    "One model id per line (empty: use the endpoint's list)",
                    cx,
                )
                .into_any_element(),
            ))
            .when_some(form.error.clone(), |this, error| {
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
                    .child(
                        button("save-provider", "Save Provider", ButtonKind::Primary, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.save_provider_form(cx))),
                    )
                    .child(
                        button("cancel-provider", "Cancel", ButtonKind::Normal, cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.provider_form = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }
}
