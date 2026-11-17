//! M9: the settings window — keys, custom providers, defaults, theme, shortcuts and
//! permissions, each applied to the running app.

use std::sync::Arc;

use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use zp_ai::{FauxProvider, Models, ThinkingLevel};
use zp_core::permissions::PermissionMode;
use zp_core::settings::{BlockDisplay, Settings, ThemeChoice, TranscriptSettings};

use crate::settings::{Page, SettingsWindow, TranscriptBlock};
use crate::store::{AppModels, AppSettings};
use crate::tests::project_dir;
use crate::tests_conversation::{active, open, setup_models, tab_count, wait_until};

fn settings_window(page: Page, cx: &mut TestAppContext) -> WindowHandle<SettingsWindow> {
    let window = cx
        .update(|cx| SettingsWindow::open_at(page, cx))
        .expect("the settings window opens");
    cx.run_until_parked();
    window
}

fn saved(data: &tempfile::TempDir) -> Settings {
    Settings::load(&zp_core::paths::settings_file(data.path())).unwrap()
}

#[gpui::test]
fn transcript_display_settings_are_saved_and_applied(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let window = settings_window(Page::Appearance, cx);
    for display in BlockDisplay::ALL {
        for block in TranscriptBlock::ALL {
            window
                .update(cx, |view, _, cx| {
                    view.set_transcript_display(block, display, cx);
                })
                .unwrap();
        }
        let expected = TranscriptSettings {
            thinking: Some(display),
            file_edits: Some(display),
            commands: Some(display),
            other_tools: Some(display),
        };
        assert_eq!(saved(&data).transcript, expected);
        assert_eq!(cx.update(|cx| AppSettings::transcript(cx)), expected);
    }
}

#[gpui::test]
fn one_settings_window(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    // Closing the last window quits; keep a project open.
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let first = settings_window(Page::General, cx);
    let windows = cx.update(|cx| cx.windows().len());
    let second = settings_window(Page::Keybindings, cx);
    assert_eq!(first, second);
    assert_eq!(cx.update(|cx| cx.windows().len()), windows);
    let page = first.update(cx, |view, _, _| view.page()).unwrap();
    assert_eq!(page, Page::Keybindings);
    // The menu action opens it too.
    first
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.windows().len()), windows - 1);
    cx.dispatch_action(project_window.into(), crate::actions::OpenSettings);
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.windows().len()), windows);
}

#[gpui::test]
fn model_prices_match_unprefixed_model_names(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    cx.update(|cx| {
        let models = &cx.global::<AppModels>().models;
        let mut canonical = models.model("faux", "faux-1").unwrap();
        canonical.id = "Catalog-Model".into();
        canonical.provider = "catalog".into();
        canonical.cost.input = 3.0;
        canonical.cost.output = 9.0;
        models.register(Arc::new(zp_ai::HttpProvider::new(
            "catalog",
            "Catalog",
            ["UNCONFIGURED_CATALOG_KEY"],
            vec![canonical.clone()],
        )));

        let custom = [
            "catalog-model",
            "CATALOG-MODEL",
            "vendor/catalog-model",
            "vendor/Catalog-Model",
            "gateway/vendor/catalog-model",
            "Gateway/Vendor/CATALOG-MODEL",
            "priced/catalog-model",
            "vendor/unknown-model",
        ]
        .into_iter()
        .map(|id| {
            let mut model = canonical.clone();
            model.id = id.into();
            model.provider = "custom".into();
            model.cost = if id == "priced/catalog-model" {
                zp_ai::ModelCost {
                    input: 5.0,
                    output: 10.0,
                    ..Default::default()
                }
            } else {
                zp_ai::ModelCost::default()
            };
            model
        })
        .collect::<Vec<_>>();
        models.register(Arc::new(
            zp_ai::HttpProvider::new("custom", "Custom", Vec::<String>::new(), custom)
                .without_api_key(),
        ));
    });
    let window = settings_window(Page::Models, cx);
    let rows = window
        .update(cx, |view, _, cx| view.model_rows(cx))
        .unwrap();
    for (id, expected) in [
        ("catalog-model", Some((3.0, 9.0))),
        ("CATALOG-MODEL", Some((3.0, 9.0))),
        ("vendor/catalog-model", Some((3.0, 9.0))),
        ("vendor/Catalog-Model", Some((3.0, 9.0))),
        ("gateway/vendor/catalog-model", Some((3.0, 9.0))),
        ("Gateway/Vendor/CATALOG-MODEL", Some((3.0, 9.0))),
        ("priced/catalog-model", Some((5.0, 10.0))),
        ("vendor/unknown-model", None),
    ] {
        let row = rows
            .iter()
            .find(|row| row.id == format!("custom/{id}"))
            .unwrap();
        assert_eq!(row.price, expected, "{id}");
    }
}

#[gpui::test]
fn api_keys_and_custom_providers(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    cx.update(|cx| {
        let models = &cx.global::<AppModels>().models;
        for provider in Models::builtin().providers() {
            models.register(provider);
        }
    });
    let models = cx.update(|cx| cx.global::<AppModels>().models.clone());
    let models = models.with_env(Arc::new(|_| None));
    let window = settings_window(Page::Providers, cx);

    // Only configured providers are listed; the dialog offers the others.
    let configured = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, cx| view.configured_providers(cx))
            .unwrap()
    };
    assert_eq!(configured(cx), ["faux"], "keyless");
    window
        .update(cx, |view, window, cx| view.open_add_dialog(window, cx))
        .unwrap();
    let offered = window
        .update(cx, |view, _, cx| {
            assert!(matches!(
                view.dialog(),
                Some(crate::settings::ProviderDialog::Choose { .. })
            ));
            view.addable_providers(cx)
        })
        .unwrap();
    assert!(
        offered
            .iter()
            .any(|(id, _, configured)| id == "deepseek" && !configured)
    );
    assert!(
        !offered.iter().any(|(id, _, _)| id == "faux"),
        "needs no key"
    );
    // Choosing one asks for its key; saving makes it available and refreshes its models.
    assert!(models.resolve_auth("deepseek").is_none());
    window
        .update(cx, |view, window, cx| {
            view.edit_key("deepseek", window, cx);
            let input = view.key_input().unwrap();
            input.update(cx, |input, cx| input.emplace("  sk-test  ", cx));
            view.save_key(cx);
        })
        .unwrap();
    assert!(
        window
            .update(cx, |view, _, _| view.dialog().is_none())
            .unwrap(),
        "closed"
    );
    assert_eq!(configured(cx), ["faux", "deepseek"]);
    let auth = models.resolve_auth("deepseek").unwrap();
    assert_eq!(auth.api_key.as_deref(), Some("sk-test"));
    assert_eq!(auth.source, zp_ai::AuthSource::Stored);
    let requests = cx.update(|cx| cx.global::<AppModels>().sync.requests.clone());
    assert_eq!(requests, [(vec!["deepseek".to_owned()], true)]);
    window
        .update(cx, |view, _, cx| view.remove_key("deepseek", cx))
        .unwrap();
    assert!(models.resolve_auth("deepseek").is_none());
    assert_eq!(configured(cx), ["faux"]);

    // A custom provider: validated, saved, registered and usable.
    window
        .update(cx, |view, window, cx| {
            view.open_provider_form(None, window, cx);
            let form = view.provider_form().unwrap();
            for (field, text) in [
                (&form.id, "local"),
                (&form.name, "Local LLM"),
                (&form.base_url, "localhost:1234"),
                (&form.headers, "X-Team: zp"),
                (&form.models, "qwen3-coder\ngpt-oss"),
            ] {
                field.update(cx, |input, cx| input.emplace(text, cx));
            }
            view.save_provider_form(cx);
        })
        .unwrap();
    let still_open = window
        .update(cx, |view, _, _| view.provider_form().is_some())
        .unwrap();
    assert!(still_open, "an invalid URL keeps the form open");
    window
        .update(cx, |view, _, cx| {
            let form = view.provider_form().unwrap();
            form.base_url.update(cx, |input, cx| {
                input.emplace("http://localhost:1234/v1", cx)
            });
            view.save_provider_form(cx);
        })
        .unwrap();
    assert!(
        window
            .update(cx, |view, _, _| view.provider_form().is_none())
            .unwrap()
    );
    assert_eq!(saved(&data).providers[0].models, ["qwen3-coder", "gpt-oss"]);
    let model = models.default_model("local").unwrap();
    assert_eq!(
        (model.id.as_str(), model.headers["X-Team"].as_str()),
        ("qwen3-coder", "zp")
    );
    assert!(models.resolve_auth("local").is_some(), "keyless");

    // Editing keeps the id; deleting removes it everywhere.
    window
        .update(cx, |view, window, cx| {
            view.open_provider_form(Some("local"), window, cx);
            let form = view.provider_form().unwrap();
            form.name
                .update(cx, |input, cx| input.emplace("Renamed", cx));
            view.save_provider_form(cx);
        })
        .unwrap();
    assert_eq!(models.provider("local").unwrap().name(), "Renamed");
    assert_eq!(saved(&data).providers.len(), 1);
    window
        .update(cx, |view, _, cx| view.delete_provider("local", cx))
        .unwrap();
    assert!(models.provider("local").is_none());
    assert!(saved(&data).providers.is_empty());
}

#[gpui::test]
fn general_defaults_apply_to_new_conversations(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], true);
    let other = FauxProvider::new("other");
    cx.update(|cx| {
        cx.global::<AppModels>()
            .models
            .register(Arc::new(other.clone()))
    });
    cx.update(|cx| AppModels::remember_model(&faux.model(), cx));
    let window = settings_window(Page::General, cx);
    let spec = other.model().qualified_id();
    window
        .update(cx, |view, _, cx| {
            view.set_default_model(Some(spec.clone()), cx);
            view.set_default_thinking(ThinkingLevel::High, cx);
            view.set_generate_titles(false, cx);
        })
        .unwrap();
    let configured = saved(&data);
    assert_eq!(configured.default_model.as_deref(), Some(spec.as_str()));
    assert_eq!(configured.default_thinking, Some(ThinkingLevel::High));
    assert!(!cx.update(|cx| cx.global::<AppModels>().generate_titles));

    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let status = active(project_window, cx).read_with(cx, |view, _| view.status());
    assert_eq!(status.model.unwrap().qualified_id(), spec);
    assert_eq!(status.thinking, ThinkingLevel::High);

    window
        .update(cx, |view, _, cx| view.set_default_model(None, cx))
        .unwrap();
    assert!(saved(&data).default_model.is_none());
    cx.dispatch_action(project_window.into(), crate::actions::NewChat);
    cx.run_until_parked();
    let status = active(project_window, cx).read_with(cx, |view, _| view.status());
    assert_eq!(
        status.model.unwrap().qualified_id(),
        faux.model().qualified_id()
    );
}

#[gpui::test]
fn theme_and_shortcuts(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let window = settings_window(Page::Appearance, cx);
    let dark =
        |cx: &mut TestAppContext| cx.update(|cx| crate::theme::ActiveTheme::theme(cx).is_dark);
    window
        .update(cx, |view, _, cx| view.set_theme(ThemeChoice::Light, cx))
        .unwrap();
    assert!(!dark(cx));
    window
        .update(cx, |view, _, cx| view.set_theme(ThemeChoice::Dark, cx))
        .unwrap();
    assert!(dark(cx));
    assert_eq!(saved(&data).theme, Some(ThemeChoice::Dark));

    // Rebind New Chat by recording keys in the settings window (saved with the
    // platform's modifier name).
    let recorded = if cfg!(target_os = "macos") {
        "cmd-shift-k"
    } else {
        "ctrl-shift-k"
    };
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    window
        .update(cx, |view, window, cx| {
            view.set_page(Page::Keybindings, cx);
            view.start_recording("zp::NewChat", window, cx);
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "secondary-shift-k");
    cx.run_until_parked();
    assert!(!window.update(cx, |view, _, _| view.is_recording()).unwrap());
    assert_eq!(saved(&data).keybindings["zp::NewChat"], recorded);
    let before = tab_count(project_window, cx);
    cx.simulate_keystrokes(project_window.into(), "secondary-shift-k");
    cx.run_until_parked();
    assert_eq!(tab_count(project_window, cx), before + 1);
    cx.simulate_keystrokes(project_window.into(), "secondary-t");
    cx.run_until_parked();
    assert_eq!(
        tab_count(project_window, cx),
        before + 1,
        "the old shortcut is gone"
    );

    // Escape cancels a recording; Reset restores the default.
    window
        .update(cx, |view, window, cx| {
            view.start_recording("zp::NewChat", window, cx)
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "escape");
    cx.run_until_parked();
    assert_eq!(saved(&data).keybindings["zp::NewChat"], recorded);
    window
        .update(cx, |view, _, cx| view.set_shortcut("zp::NewChat", None, cx))
        .unwrap();
    cx.simulate_keystrokes(project_window.into(), "secondary-t");
    cx.run_until_parked();
    assert_eq!(tab_count(project_window, cx), before + 2);
}

#[gpui::test]
fn permission_settings_apply_to_open_projects(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let conversation = active(project_window, cx);
    assert_eq!(
        conversation.read_with(cx, |v, _| v.permission_mode()),
        PermissionMode::FullAuto
    );
    let window = settings_window(Page::Permissions, cx);
    window
        .update(cx, |view, window, cx| {
            view.set_permission_mode(PermissionMode::AutoEdit, window, cx)
        })
        .unwrap();
    assert_eq!(
        conversation.read_with(cx, |v, _| v.permission_mode()),
        PermissionMode::AutoEdit
    );

    // Lists are saved with the button; unavailable review models are refused.
    let [commands, _directories] = window
        .update(cx, |view, _, _| view.permission_inputs())
        .unwrap();
    commands.update(cx, |input, cx| {
        input.emplace("cargo test\n\n git status ", cx)
    });
    window
        .update(cx, |view, _, cx| {
            view.set_review_model("nope/nothing", false, cx)
        })
        .unwrap();
    let error = window
        .update(cx, |view, _, _| view.notice().cloned())
        .unwrap();
    assert!(matches!(error, Some((text, true)) if text.contains("unavailable review model")));
    window
        .update(cx, |view, _, cx| view.save_permission_lists(cx))
        .unwrap();
    assert_eq!(
        saved(&data).permissions.allow_commands,
        ["cargo test", "git status"]
    );
    let policy = cx.update(|cx| crate::approvals::project_policy(&project, cx));
    assert_eq!(policy.allow_commands, ["cargo test", "git status"]);
    assert_eq!(
        cx.update(|cx| AppSettings::get(cx).permissions.mode),
        Some(PermissionMode::AutoEdit)
    );
}

#[gpui::test]
fn review_model_is_chosen_from_settings_dialog(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let conversation = active(project_window, cx);
    let window = settings_window(Page::Permissions, cx);
    window
        .update(cx, |view, window, cx| {
            view.set_permission_mode(PermissionMode::AutoReview, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        window
            .update(cx, |view, _, _| view.review_model_picker().is_some())
            .unwrap()
    );
    assert_eq!(
        saved(&data).permissions.mode,
        Some(PermissionMode::FullAuto)
    );
    cx.simulate_keystrokes(window.into(), "escape");
    cx.run_until_parked();
    assert!(
        window
            .update(cx, |view, _, _| view.review_model_picker().is_none())
            .unwrap()
    );
    assert_eq!(saved(&data).permissions.review_model, None);

    // Choosing the review model alone leaves the mode unchanged.
    window
        .update(cx, |view, window, cx| {
            view.open_review_model_picker(false, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_input(window.into(), "faux-1");
    cx.run_until_parked();
    {
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let option = visual.debug_bounds("review-model-0").unwrap();
        visual.simulate_click(option.center(), Modifiers::none());
    }
    cx.run_until_parked();
    assert_eq!(
        saved(&data).permissions.review_model,
        Some(faux.model().qualified_id())
    );
    assert_eq!(
        saved(&data).permissions.mode,
        Some(PermissionMode::FullAuto)
    );

    // Switching mode still asks for a model, with the saved model marked.
    window
        .update(cx, |view, window, cx| {
            view.set_permission_mode(PermissionMode::AutoReview, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        window
            .update(cx, |view, _, _| view.review_model_picker().is_some())
            .unwrap()
    );
    cx.simulate_keystrokes(window.into(), "enter");
    cx.run_until_parked();
    assert_eq!(
        saved(&data).permissions.mode,
        Some(PermissionMode::AutoReview)
    );
    assert_eq!(
        conversation.read_with(cx, |view, _| view.permission_mode()),
        PermissionMode::AutoReview
    );
}

#[gpui::test]
fn signing_in_to_providers(cx: &mut TestAppContext) {
    use crate::settings::ProviderDialog;
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    cx.update(|cx| {
        let models = &cx.global::<AppModels>().models;
        for provider in Models::builtin().providers() {
            models.register(provider);
        }
    });
    let window = settings_window(Page::Providers, cx);
    let dialog_kind = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| match view.dialog() {
                Some(ProviderDialog::Key { .. }) => "key",
                Some(ProviderDialog::Method { .. }) => "method",
                Some(ProviderDialog::Login(_)) => "login",
                Some(_) => "other",
                None => "none",
            })
            .unwrap()
    };

    // Key only, key or sign-in, sign-in only.
    for (provider, kind) in [
        ("deepseek", "key"),
        ("anthropic", "method"),
        ("openrouter", "method"),
    ] {
        window
            .update(cx, |view, window, cx| {
                view.choose_provider(provider, window, cx)
            })
            .unwrap();
        assert_eq!(dialog_kind(cx), kind, "{provider}");
    }

    // Signing in shows the page to open; a refused authorization is reported.
    window
        .update(cx, |view, window, cx| {
            view.start_login("openrouter", window, cx)
        })
        .unwrap();
    assert_eq!(dialog_kind(cx), "login");
    let login_url = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| view.login_state().and_then(|(url, _)| url))
            .unwrap()
    };
    wait_until(cx, |cx| login_url(cx).is_some());
    let url = login_url(cx).unwrap();
    assert!(url.starts_with("https://openrouter.ai/auth?"), "{url}");
    // The callback the browser would be sent to: http://localhost:<port>/<path>.
    let callback = url
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("callback_url="))
        .unwrap()
        .replace("%3A", ":")
        .replace("%2F", "/");
    let (port, path) = callback
        .strip_prefix("http://localhost:")
        .and_then(|rest| rest.split_once('/'))
        .unwrap();
    let (port, path): (u16, String) = (port.parse().unwrap(), format!("/{path}"));
    {
        use std::io::{Read as _, Write as _};
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET {path}?error=access_denied HTTP/1.1\r\nhost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    }
    let login_error = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| {
                view.login_state().and_then(|(_, error)| error)
            })
            .unwrap()
    };
    wait_until(cx, |cx| login_error(cx).is_some());
    assert!(login_error(cx).unwrap().contains("access_denied"));
    window
        .update(cx, |view, _, cx| view.close_dialog(cx))
        .unwrap();

    // A stored sign-in counts as configured until signing out.
    let models = cx.update(|cx| cx.global::<AppModels>().models.clone());
    cx.update(|cx| {
        cx.global::<AppModels>()
            .credentials
            .set_credential(
                "openai-codex",
                &zp_ai::Credential::OAuth(zp_ai::OAuthCredential::new("t", "r", 3600.0, 0)),
            )
            .unwrap()
    });
    assert!(models.is_signed_in("openai-codex"));
    let configured = window
        .update(cx, |view, _, cx| view.configured_providers(cx))
        .unwrap();
    assert!(configured.contains(&"openai-codex".to_owned()));
    window
        .update(cx, |view, _, cx| view.sign_out("openai-codex", cx))
        .unwrap();
    assert!(!models.is_signed_in("openai-codex"));
}
