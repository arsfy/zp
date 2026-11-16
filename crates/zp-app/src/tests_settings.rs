//! M9: the settings window — keys, custom providers, defaults, theme, shortcuts and
//! permissions, each applied to the running app.

use std::sync::Arc;

use gpui::{TestAppContext, WindowHandle};
use zp_ai::{FauxProvider, Models, ThinkingLevel};
use zp_core::permissions::PermissionMode;
use zp_core::settings::{Settings, ThemeChoice};

use crate::settings::{Page, SettingsWindow};
use crate::store::{AppModels, AppSettings};
use crate::tests::project_dir;
use crate::tests_conversation::{active, open, setup_models, tab_count};

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
    let window = settings_window(Page::General, cx);
    let spec = other.model().qualified_id();
    window
        .update(cx, |view, _, cx| {
            view.set_default_model(Some(spec.clone()), cx);
            view.set_default_thinking(ThinkingLevel::High, cx);
            view.set_generate_titles(false, cx);
        })
        .unwrap();
    let saved = saved(&data);
    assert_eq!(saved.default_model.as_deref(), Some(spec.as_str()));
    assert_eq!(saved.default_thinking, Some(ThinkingLevel::High));
    assert!(!cx.update(|cx| cx.global::<AppModels>().generate_titles));

    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let status = active(project_window, cx).read_with(cx, |view, _| view.status());
    assert_eq!(status.model.unwrap().qualified_id(), spec);
    assert_eq!(status.thinking, ThinkingLevel::High);
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

    // Rebind New Chat by recording keys in the settings window.
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    window
        .update(cx, |view, window, cx| {
            view.set_page(Page::Keybindings, cx);
            view.start_recording("zp::NewChat", window, cx);
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "cmd-shift-k");
    cx.run_until_parked();
    assert!(!window.update(cx, |view, _, _| view.is_recording()).unwrap());
    assert_eq!(saved(&data).keybindings["zp::NewChat"], "cmd-shift-k");
    let before = tab_count(project_window, cx);
    cx.simulate_keystrokes(project_window.into(), "cmd-shift-k");
    cx.run_until_parked();
    assert_eq!(tab_count(project_window, cx), before + 1);
    cx.simulate_keystrokes(project_window.into(), "cmd-t");
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
    assert_eq!(saved(&data).keybindings["zp::NewChat"], "cmd-shift-k");
    window
        .update(cx, |view, _, cx| view.set_shortcut("zp::NewChat", None, cx))
        .unwrap();
    cx.simulate_keystrokes(project_window.into(), "cmd-t");
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
        .update(cx, |view, _, cx| {
            view.set_permission_mode(PermissionMode::AutoEdit, cx)
        })
        .unwrap();
    assert_eq!(
        conversation.read_with(cx, |v, _| v.permission_mode()),
        PermissionMode::AutoEdit
    );

    // Lists are saved with the button; an unknown review model is refused.
    let [review, commands, _directories] = window
        .update(cx, |view, _, _| view.permission_inputs())
        .unwrap();
    review.update(cx, |input, cx| input.emplace("nope/nothing", cx));
    commands.update(cx, |input, cx| {
        input.emplace("cargo test\n\n git status ", cx)
    });
    window
        .update(cx, |view, _, cx| view.save_permission_lists(cx))
        .unwrap();
    let error = window
        .update(cx, |view, _, _| view.notice().cloned())
        .unwrap();
    assert!(matches!(error, Some((text, true)) if text.contains("unknown review model")));
    review.update(cx, |input, cx| input.emplace("", cx));
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
