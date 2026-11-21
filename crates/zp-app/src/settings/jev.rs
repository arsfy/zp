use gpui::{AnyElement, Context, Entity, IntoElement, div, prelude::*};
use gpui_elements::editable_text::EditableTextState;
use zp_core::permissions::jev::{DEFAULT_BASE_URL, DEFAULT_MODEL, JevSettings, credential_id};
use zp_core::permissions::{PermissionMode, ReviewBackend};

use super::{
    ButtonKind, SettingsWindow, button, input, secret_field, section, segmented, text_field,
};
use crate::store::{AppModels, AppSettings};

#[derive(Clone, Copy)]
pub(super) enum OnJevSave {
    Configure,
    EnableAutoReview,
}

pub(super) struct JevForm {
    base_url: Entity<EditableTextState>,
    model: Entity<EditableTextState>,
    api_key: Entity<EditableTextState>,
    on_save: OnJevSave,
}

impl SettingsWindow {
    pub(crate) fn open_jev_configuration(&mut self, cx: &mut Context<Self>) {
        if self.jev_form.is_none() {
            self.show_jev_configuration(OnJevSave::Configure, cx);
        }
    }

    pub(super) fn show_jev_configuration(&mut self, on_save: OnJevSave, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx)
            .permissions
            .review_jev
            .clone()
            .unwrap_or_default();
        self.jev_form = Some(JevForm {
            base_url: input(settings.base_url, cx),
            model: input(settings.model, cx),
            api_key: input("", cx),
            on_save,
        });
        cx.notify();
    }

    /// Switches the reviewer at once; Jev without a saved connection also opens its form.
    fn select_review_backend(&mut self, backend: ReviewBackend, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |settings| {
            settings.permissions.review_backend = Some(backend);
        });
        let needs_connection = backend == ReviewBackend::Jev
            && !AppSettings::get(cx)
                .permissions
                .review_jev
                .as_ref()
                .is_some_and(|config| config.endpoint().is_ok());
        if needs_connection {
            if self.jev_form.is_none() {
                self.show_jev_configuration(OnJevSave::Configure, cx);
            }
        } else if backend == ReviewBackend::Llm {
            self.jev_form = None;
        }
        self.report(result, cx);
    }

    fn save_jev_configuration(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.jev_form else {
            return;
        };
        let settings = JevSettings {
            base_url: form.base_url.read(cx).as_str().trim().to_owned(),
            model: form.model.read(cx).as_str().trim().to_owned(),
        };
        let on_save = form.on_save;
        let result = (|| {
            let endpoint = settings.endpoint()?;
            let key = form.api_key.read(cx).as_str().trim();
            if !key.is_empty() {
                cx.global::<AppModels>()
                    .credentials
                    .set(&credential_id(&endpoint), key)?;
            }
            AppSettings::update(cx, |app| {
                app.permissions.review_jev = Some(settings);
                app.permissions.review_backend = Some(ReviewBackend::Jev);
                if matches!(on_save, OnJevSave::EnableAutoReview) {
                    app.permissions.mode = Some(PermissionMode::AutoReview);
                }
            })
        })();
        if result.is_ok() {
            self.jev_form = None;
        }
        self.report(result, cx);
    }

    pub(super) fn render_review_backend(
        &self,
        backend: ReviewBackend,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(segmented(
                "review-backend",
                [("LLM", ReviewBackend::Llm), ("Jev", ReviewBackend::Jev)]
                    .into_iter()
                    .map(|(label, option)| (label.to_owned(), backend == option)),
                cx,
                |this, index, cx| {
                    this.select_review_backend([ReviewBackend::Llm, ReviewBackend::Jev][index], cx)
                },
            ))
            .when(
                backend == ReviewBackend::Jev && self.jev_form.is_none(),
                |row| {
                    row.child(
                        button("configure-jev", "Configure Jev…", ButtonKind::Normal, cx)
                            .debug_selector(|| "configure-jev".into())
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_jev_configuration(cx)),
                            ),
                    )
                },
            )
            .into_any_element()
    }

    pub(super) fn render_jev_configuration(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.jev_form.as_ref()?;
        Some(div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section("Jev connection", "Jev receives the task and proposed tool call at this URL.", cx))
            .child(div().child("Base URL").child(text_field(&form.base_url, DEFAULT_BASE_URL, cx)))
            .child(div().child("Model name").child(text_field(&form.model, DEFAULT_MODEL, cx)))
            .child(div().child(section("API key", "Leave blank to keep the saved key. OpenRouter can also use your existing provider key.", cx))
                .child(div().key_context("ApiKeyInput").child(secret_field(&form.api_key, "Optional", cx))))
            .child(div().flex().gap_2()
                .child(button("save-jev", "Save Jev settings", ButtonKind::Primary, cx)
                    .debug_selector(|| "save-jev".into())
                    .on_click(cx.listener(|this, _, _, cx| this.save_jev_configuration(cx))))
                .child(button("cancel-jev", "Cancel", ButtonKind::Normal, cx)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.jev_form = None;
                        this.report(Ok(()), cx);
                    }))))
            .into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use zp_ai::FauxProvider;

    use super::*;
    use crate::settings::Page;
    use crate::tests::project_dir;
    use crate::tests_conversation::{active, open, setup_models};

    #[gpui::test]
    fn configures_jev_and_switches_the_reviewer(cx: &mut TestAppContext) {
        let faux = FauxProvider::default();
        let data = setup_models(cx, &[&faux], false);
        let settings = cx
            .update(|cx| SettingsWindow::open_at(Page::Permissions, cx))
            .unwrap();
        VisualTestContext::from_window(settings.into(), cx)
            .simulate_resize(gpui::size(gpui::px(980.), gpui::px(1100.)));
        let click = |selector: &'static str, cx: &mut TestAppContext| {
            cx.run_until_parked();
            let mut visual = VisualTestContext::from_window(settings.into(), cx);
            let bounds = visual.debug_bounds(selector).unwrap();
            visual.simulate_click(bounds.center(), Modifiers::none());
            cx.run_until_parked();
        };
        let shown = |selector: &'static str, cx: &mut TestAppContext| {
            cx.run_until_parked();
            VisualTestContext::from_window(settings.into(), cx)
                .debug_bounds(selector)
                .is_some()
        };
        let backend = |cx: &mut TestAppContext| {
            cx.update(|cx| AppSettings::get(cx).permissions.review_backend)
        };
        let form_open = |cx: &mut TestAppContext| {
            settings
                .update(cx, |view, _, _| view.jev_form.is_some())
                .unwrap()
        };

        // LLM is the default: its model picker shows, Jev's controls do not.
        assert!(shown("review-model-button", cx));
        assert!(!shown("configure-jev", cx));

        // Without a saved connection, choosing Jev switches at once, hides the LLM model
        // picker and asks for the connection; going back to LLM undoes all of it.
        click("review-backend-1", cx);
        assert_eq!(backend(cx), Some(ReviewBackend::Jev));
        assert!(form_open(cx));
        assert!(!shown("review-model-button", cx));
        click("review-backend-0", cx);
        assert_eq!(backend(cx), Some(ReviewBackend::Llm));
        assert!(!form_open(cx));
        assert!(shown("review-model-button", cx));
        assert!(!shown("configure-jev", cx));

        click("review-backend-1", cx);
        assert!(form_open(cx));
        settings
            .update(cx, |view, _, cx| {
                let form = view.jev_form.as_ref().unwrap();
                form.base_url.update(cx, |field, cx| {
                    field.emplace("http://localhost:8080/api", cx)
                });
                form.model
                    .update(cx, |field, cx| field.emplace("local-jev", cx));
                form.api_key
                    .update(cx, |field, cx| field.emplace("jev-fixture", cx));
            })
            .unwrap();
        click("save-jev", cx);
        settings
            .update(cx, |view, _, _| {
                assert!(
                    view.jev_form.is_none(),
                    "configuration was not saved: {:?}",
                    view.notice()
                );
            })
            .unwrap();
        let settings_file = zp_core::paths::settings_file(data.path());
        let saved = std::fs::read_to_string(&settings_file).unwrap();
        assert!(!saved.contains("jev-fixture"));
        // Once connected, Jev offers to change its connection instead of the LLM picker.
        assert!(shown("configure-jev", cx));
        assert!(!shown("review-model-button", cx));

        let saved = zp_core::settings::Settings::load(&settings_file).unwrap();
        assert_eq!(saved.permissions.review_backend, Some(ReviewBackend::Jev));
        let config = saved.permissions.review_jev.unwrap();
        assert_eq!(config.model, "local-jev");
        assert_eq!(config.base_url, "http://localhost:8080/api");
        cx.update(|cx| {
            let stored = cx
                .global::<AppModels>()
                .models
                .stored_api_key(&credential_id(&config.endpoint().unwrap()));
            assert_eq!(stored.as_deref(), Some("jev-fixture"));
        });

        let (_dir, project) = project_dir();
        let window = open(cx, &project);
        let conversation = active(window, cx);
        window
            .update(cx, |_, window, cx| {
                conversation.update(cx, |view, cx| {
                    view.select_permission_mode(PermissionMode::AutoReview, window, cx)
                });
            })
            .unwrap();
        assert_eq!(
            conversation.read_with(cx, |view, _| view.permission_mode()),
            PermissionMode::AutoReview
        );
        assert!(conversation.read_with(cx, |view, _| view.review_model_picker.is_none()));

        click("review-backend-0", cx);
        let saved = zp_core::settings::Settings::load(&settings_file).unwrap();
        assert_eq!(saved.permissions.review_backend, Some(ReviewBackend::Llm));
        assert_eq!(saved.permissions.review_jev, Some(config.clone()));
        assert!(shown("review-model-button", cx));

        // With a saved connection, Jev is chosen without opening the form again.
        click("review-backend-1", cx);
        assert_eq!(backend(cx), Some(ReviewBackend::Jev));
        assert!(!form_open(cx));
        assert!(!shown("review-model-button", cx));
        assert!(shown("configure-jev", cx));
    }
}
