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
fn subagent_assignment_modes_can_be_switched_in_settings(cx: &mut TestAppContext) {
    use zp_ai::FauxResponse;
    use zp_core::subagents::{Approval, Assignment, Profile, Report};

    let faux = FauxProvider::default();
    let child = FauxProvider::new("child");
    let data = setup_models(cx, &[&faux, &child], false);
    let (_dir, project) = project_dir();
    let window = open(cx, &project);
    let conversation = active(window, cx);
    let settings = settings_window(Page::Subagents, cx);
    let click = |selector, cx: &mut TestAppContext| {
        let mut visual = VisualTestContext::from_window(settings.into(), cx);
        let bounds = visual
            .debug_bounds(selector)
            .expect("settings control is visible");
        visual.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    };

    click("enable-subagents", cx);
    assert!(saved(&data).subagents.unwrap().is_enabled());
    cx.update(|cx| {
        AppSettings::update(cx, |settings| {
            let settings = settings.subagents.as_mut().unwrap();
            settings.approval = Some(Approval::Allowlisted);
            settings.default_profile = Some("explore".into());
            settings.profiles.insert(
                "explore".into(),
                Profile {
                    model: child.model().qualified_id(),
                    description: "Find code".into(),
                    thinking: ThinkingLevel::Low,
                },
            );
        })
        .unwrap()
    });
    click("task-profiles", cx);
    assert_eq!(
        saved(&data).subagents.unwrap().assignment(),
        Assignment::TaskProfiles
    );
    faux.push(FauxResponse::tool_call(
        "subagent",
        serde_json::json!({"task": "Find code"}),
    ));
    child.push(FauxResponse::text("Found code in the workspace"));
    faux.push(FauxResponse::text("Done"));
    crate::tests_conversation::send(window, "Use a profile", cx);
    crate::tests_conversation::idle(&conversation, 3, cx);
    let report = conversation.read_with(cx, |view, _| {
        let call = view
            .transcript()
            .assistant(1)
            .unwrap()
            .tool_calls()
            .next()
            .unwrap();
        Report::from_details(
            view.transcript()
                .tool(&call.id)
                .unwrap()
                .details
                .as_ref()
                .unwrap(),
        )
        .unwrap()
    });
    assert_eq!(report.model, "child/faux-1");
    assert_eq!(
        conversation.read_with(cx, |view, _| view.status().usage.input),
        30
    );

    click("follow-main-model", cx);
    assert_eq!(
        saved(&data).subagents.unwrap().assignment(),
        Assignment::FollowMainModel
    );
    assert!(
        saved(&data)
            .subagents
            .unwrap()
            .profiles
            .contains_key("explore")
    );
}

#[gpui::test]
fn one_settings_window(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    // Closing the last window quits; keep a project open.
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let first = settings_window(Page::General, cx);
    first
        .update(cx, |_, window, cx| {
            assert_eq!(
                window.window_bounds().get_bounds().center(),
                window.display(cx).unwrap().bounds().center(),
                "Settings should be centered without the project window's cascade offset"
            );
        })
        .unwrap();
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
    settings_window(Page::General, cx)
        .update(cx, |_, window, cx| {
            assert_eq!(
                window.window_bounds().get_bounds().center(),
                window.display(cx).unwrap().bounds().center()
            );
        })
        .unwrap();
}

#[gpui::test]
fn about_action_opens_settings_and_update_preferences_are_saved(cx: &mut TestAppContext) {
    let data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_dir, project) = project_dir();
    let project_window = open(cx, &project);
    let settings = settings_window(Page::General, cx);
    cx.dispatch_action(project_window.into(), crate::actions::About);
    cx.run_until_parked();
    assert_eq!(
        settings.update(cx, |view, _, _| view.page()).unwrap(),
        Page::About
    );
    cx.update(|cx| {
        AppSettings::update(cx, |settings| {
            settings.update_feed = Some("http://127.0.0.1:1/releases".into())
        })
        .unwrap();
    });
    settings
        .update(cx, |view, _, cx| {
            view.set_auto_updates(false, cx);
            view.set_update_channel(zp_core::update::UpdateChannel::Preview, cx);
        })
        .unwrap();
    assert_eq!(saved(&data).check_for_updates, Some(false));
    assert_eq!(
        saved(&data).update_channel,
        Some(zp_core::update::UpdateChannel::Preview)
    );
    assert_eq!(
        settings.update(cx, |view, _, _| view.page()).unwrap(),
        Page::About
    );
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

/// A tiny endpoint on `runtime` that lists two models when called with key `sk-good`:
/// its base URL and the number of requests it received.
fn serve_model_list(
    runtime: &tokio::runtime::Runtime,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = requests.clone();
    runtime.spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buffer = [0u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
            let (status, body) = if !request.starts_with("get /v1/models ") {
                ("404 Not Found", r#"{"error":"no such route"}"#.to_owned())
            } else if request.contains("authorization: bearer sk-good") {
                (
                    "200 OK",
                    serde_json::json!({"data": [
                        {"id": "chat-a", "context_length": 200000, "supported_endpoints": ["/chat/completions"]},
                        {"id": "text-embedding-x"}
                    ]})
                    .to_string(),
                )
            } else {
                ("401 Unauthorized", r#"{"error":{"message":"bad key"}}"#.to_owned())
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (base_url, requests)
}

/// Types `key` and `url` into the open provider form, clicks Test and waits for the result.
fn test_endpoint(
    window: WindowHandle<SettingsWindow>,
    key: &str,
    url: &str,
    cx: &mut TestAppContext,
) -> crate::settings::EndpointTest {
    window
        .update(cx, |view, _, cx| {
            let form = view.provider_form().unwrap();
            for (field, text) in [(&form.base_url, url), (&form.key, key)] {
                field.update(cx, |input, cx| input.emplace(text, cx));
            }
            view.test_provider_form(cx);
        })
        .unwrap();
    let result = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| {
                view.provider_form().unwrap().endpoint_test().cloned()
            })
            .unwrap()
    };
    wait_until(cx, |cx| {
        !matches!(result(cx), Some(crate::settings::EndpointTest::Running))
    });
    result(cx).unwrap()
}

#[gpui::test]
fn custom_provider_form_tests_the_model_list(cx: &mut TestAppContext) {
    use crate::settings::EndpointTest;
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (base_url, _) = serve_model_list(&runtime);

    let window = settings_window(Page::Providers, cx);
    window
        .update(cx, |view, window, cx| {
            view.open_provider_form(None, window, cx)
        })
        .unwrap();
    let test = |key: &str, url: &str, cx: &mut TestAppContext| test_endpoint(window, key, url, cx);

    match test("", "localhost:1", cx) {
        EndpointTest::Failed(error) => assert!(error.contains("base URL"), "{error}"),
        other => panic!("expected a validation error, got {other:?}"),
    }
    match test("sk-bad", &base_url, cx) {
        EndpointTest::Failed(error) => assert!(error.contains("bad key"), "{error}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    match test("sk-good", &base_url, cx) {
        EndpointTest::Listed(models) => {
            let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, ["chat-a", "text-embedding-x"]);
            assert_eq!(models[0].context_window, Some(200_000));
        }
        other => panic!("expected models, got {other:?}"),
    }
    // Testing saves nothing.
    assert!(saved(&data).providers.is_empty());
    assert!(
        cx.update(|cx| cx.global::<AppModels>().models.provider("test"))
            .is_none()
    );
    drop(runtime);
}

/// Saves custom provider `id` at `base_url` with key `key` (without the form).
fn add_custom_provider(id: &str, base_url: &str, key: &str, cx: &mut TestAppContext) {
    let def = zp_core::settings::CustomProvider {
        id: id.to_owned(),
        name: id.to_owned(),
        base_url: base_url.to_owned(),
        ..Default::default()
    };
    cx.update(|cx| {
        AppSettings::update(cx, |settings| settings.providers.push(def.clone())).unwrap();
        let app = cx.global::<AppModels>();
        app.models
            .register(zp_core::providers::custom_provider(&def).unwrap());
        app.credentials.set(id, key).unwrap();
    });
}

/// Testing an edited provider sends its stored key only to the base URL it was saved for.
#[gpui::test]
fn editing_a_custom_provider_tests_a_changed_base_url_only_with_a_typed_key(
    cx: &mut TestAppContext,
) {
    use crate::settings::EndpointTest;
    use std::sync::atomic::Ordering;
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (base_url, requests) = serve_model_list(&runtime);
    add_custom_provider("acme", &base_url, "sk-good", cx);

    let window = settings_window(Page::Providers, cx);
    window
        .update(cx, |view, window, cx| {
            view.open_provider_form(Some("acme"), window, cx)
        })
        .unwrap();
    // The saved base URL (give or take a trailing `/`) gets the stored key.
    let listed = test_endpoint(window, "", &format!(" {base_url}/ "), cx);
    assert!(matches!(listed, EndpointTest::Listed(_)), "{listed:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    // Another base URL does not, and nothing is sent.
    let changed = base_url.replace("127.0.0.1", "localhost");
    match test_endpoint(window, "", &changed, cx) {
        EndpointTest::Failed(error) => assert!(error.contains("Enter the API key"), "{error}"),
        other => panic!("expected the key to be asked for, got {other:?}"),
    }
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    // A typed key works there.
    let listed = test_endpoint(window, "sk-good", &changed, cx);
    assert!(matches!(listed, EndpointTest::Listed(_)), "{listed:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    drop(runtime);
}

/// The form warns (without blocking) about secrets in the plain-text headers and about a
/// key sent over plain http:// to a remote host.
#[gpui::test]
fn custom_provider_form_warns_about_secrets_in_plain_text(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let _data = setup_models(cx, &[&faux], false);
    let window = settings_window(Page::Providers, cx);
    let fill = |fields: &[(&str, &str)], cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, cx| {
                let form = view.provider_form().unwrap();
                for (name, text) in fields {
                    let field = match *name {
                        "base_url" => &form.base_url,
                        "key" => &form.key,
                        "headers" => &form.headers,
                        other => panic!("no field {other}"),
                    };
                    field.update(cx, |input, cx| input.emplace(text, cx));
                }
            })
            .unwrap();
        cx.run_until_parked();
        let shown = |selector| {
            VisualTestContext::from_window(window.into(), cx)
                .debug_bounds(selector)
                .is_some()
        };
        let shown = (
            shown("provider-headers-hint"),
            shown("provider-headers-warning"),
            shown("provider-http-warning"),
        );
        let warnings = window
            .update(cx, |view, _, cx| {
                let form = view.provider_form().unwrap();
                (form.secret_headers_warning(cx), form.plain_http_warning(cx))
            })
            .unwrap();
        (warnings, shown)
    };
    window
        .update(cx, |view, window, cx| {
            view.open_provider_form(None, window, cx)
        })
        .unwrap();

    let ((headers, http), shown) = fill(
        &[
            ("base_url", "http://llm.example.com/v1"),
            ("headers", "X-Team: zp"),
        ],
        cx,
    );
    assert_eq!((headers, http), (None, None), "no secret header, no key");
    assert_eq!(shown, (true, false, false), "the hint only");

    let ((headers, http), shown) = fill(
        &[
            ("key", "sk-typed"),
            ("headers", "X-Team: zp\nAuthorization: Bearer sk-1"),
        ],
        cx,
    );
    let headers = headers.expect("a secret header");
    assert!(headers.contains("`Authorization`"), "{headers}");
    assert!(!headers.contains("X-Team"), "{headers}");
    assert!(
        http.expect("a typed key over http")
            .contains("llm.example.com")
    );
    assert_eq!(shown, (false, true, true));

    let ((_, http), shown) = fill(&[("base_url", "http://localhost:11434/v1")], cx);
    assert_eq!(http, None, "a local server");
    assert!(!shown.2);

    // A stored key counts too.
    add_custom_provider("acme", "http://localhost:11434/v1", "sk-stored", cx);
    window
        .update(cx, |view, window, cx| {
            view.open_provider_form(Some("acme"), window, cx)
        })
        .unwrap();
    let ((_, http), shown) = fill(&[("base_url", "http://llm.example.com/v1")], cx);
    assert!(http.is_some(), "a stored key over http");
    assert!(shown.2);
}

/// A keychain that refuses to store keys until told otherwise (like a denied macOS
/// keychain prompt).
#[derive(Default)]
struct FlakyKeychain {
    refuse: std::sync::atomic::AtomicBool,
    secrets: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl zp_core::credentials::SecretBackend for FlakyKeychain {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        Ok(self.secrets.lock().unwrap().get(provider).cloned())
    }
    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("keychain access denied");
        }
        self.secrets
            .lock()
            .unwrap()
            .insert(provider.to_owned(), secret.to_owned());
        Ok(())
    }
    fn delete(&self, provider: &str) -> anyhow::Result<()> {
        self.secrets.lock().unwrap().remove(provider);
        Ok(())
    }
}

/// When the keychain refuses the key, nothing is saved, so saving again works once it
/// allows it (instead of failing with "already exists").
#[gpui::test]
fn a_new_custom_provider_can_be_saved_again_after_the_keychain_refuses(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let keychain = Arc::new(FlakyKeychain::default());
    keychain
        .refuse
        .store(true, std::sync::atomic::Ordering::SeqCst);
    cx.update(|cx| {
        let credentials = Arc::new(zp_core::credentials::Credentials::new(
            keychain.clone(),
            None,
        ));
        let app = cx.global_mut::<AppModels>();
        app.models = app.models.clone().with_credentials(credentials.clone());
        app.credentials = credentials;
    });
    let window = settings_window(Page::Providers, cx);
    let save = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, window, cx| {
                if view.provider_form().is_none() {
                    view.open_provider_form(None, window, cx);
                }
                let form = view.provider_form().unwrap();
                for (field, text) in [
                    (&form.id, "acme"),
                    (&form.name, "Acme"),
                    (&form.base_url, "https://llm.acme.test/v1"),
                    (&form.models, "acme-chat"),
                    (&form.key, "sk-acme"),
                ] {
                    field.update(cx, |input, cx| input.emplace(text, cx));
                }
                view.save_provider_form(cx);
                view.provider_form()
                    .map(|form| form.error().unwrap_or_default().to_owned())
            })
            .unwrap()
    };
    let registered = |cx: &mut TestAppContext| {
        cx.update(|cx| cx.global::<AppModels>().models.provider("acme"))
            .is_some()
    };

    let error = save(cx).expect("the form stays open");
    assert!(error.contains("keychain access denied"), "{error}");
    assert!(saved(&data).providers.is_empty(), "nothing saved");
    assert!(!registered(cx));

    keychain
        .refuse
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(save(cx), None, "saved and closed");
    assert_eq!(saved(&data).providers[0].id, "acme");
    assert!(registered(cx));
    assert_eq!(
        keychain
            .secrets
            .lock()
            .unwrap()
            .get("acme")
            .map(String::as_str),
        Some("sk-acme")
    );
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
    // A change that worked announces nothing and clears the earlier error.
    assert_eq!(
        window
            .update(cx, |view, _, _| view.notice().cloned())
            .unwrap(),
        None
    );
    let policy = cx.update(|cx| crate::approvals::project_policy(&project, cx));
    assert_eq!(policy.allow_commands, ["cargo test", "git status"]);
    assert_eq!(policy.review_thinking, ThinkingLevel::Off);
    assert_eq!(
        cx.update(|cx| AppSettings::get(cx).permissions.mode),
        Some(PermissionMode::AutoEdit)
    );

    // The review model's thinking level.
    window
        .update(cx, |view, _, cx| {
            view.set_review_thinking(ThinkingLevel::Medium, cx)
        })
        .unwrap();
    assert_eq!(
        saved(&data).permissions.review_thinking,
        Some(ThinkingLevel::Medium)
    );
    let policy = cx.update(|cx| crate::approvals::project_policy(&project, cx));
    assert_eq!(policy.review_thinking, ThinkingLevel::Medium);
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

    // Switching mode reuses the saved model without opening the picker.
    window
        .update(cx, |view, window, cx| {
            view.set_permission_mode(PermissionMode::AutoReview, window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        window
            .update(cx, |view, _, _| view.review_model_picker().is_none())
            .unwrap()
    );
    assert_eq!(
        saved(&data).permissions.mode,
        Some(PermissionMode::AutoReview)
    );
    assert_eq!(
        conversation.read_with(cx, |view, _| view.permission_mode()),
        PermissionMode::AutoReview
    );

    for mode in [PermissionMode::Ask, PermissionMode::AutoReview] {
        window
            .update(cx, |view, window, cx| {
                view.set_permission_mode(mode, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            window
                .update(cx, |view, _, _| view.review_model_picker().is_none())
                .unwrap()
        );
        assert_eq!(saved(&data).permissions.mode, Some(mode));
        assert_eq!(
            saved(&data).permissions.review_model,
            Some(faux.model().qualified_id())
        );
    }
}

#[gpui::test]
fn review_mode_in_settings_prompts_again_if_the_saved_model_is_unavailable(
    cx: &mut TestAppContext,
) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    cx.update(|cx| {
        AppSettings::update(cx, |settings| {
            settings.permissions.mode = Some(PermissionMode::Ask);
            settings.permissions.review_model = Some("unavailable/model".into());
        })
        .unwrap();
    });
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
    assert_eq!(saved(&data).permissions.mode, Some(PermissionMode::Ask));
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

#[gpui::test]
fn subagent_limits_apply_as_typed_and_profiles_keep_their_model_when_edited(
    cx: &mut TestAppContext,
) {
    use zp_core::subagents::{Assignment, Profile};

    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    cx.update(|cx| {
        AppSettings::update(cx, |settings| {
            let subagents = settings.subagents.get_or_insert_default();
            subagents.enabled = Some(true);
            subagents.assignment = Some(Assignment::TaskProfiles);
            subagents.profiles.insert(
                "explore".into(),
                Profile {
                    model: "gone/model".into(),
                    description: "Find code".into(),
                    thinking: ThinkingLevel::Low,
                },
            );
        })
        .unwrap()
    });
    let window = settings_window(Page::Subagents, cx);
    let fields = window
        .update(cx, |view, _, _| view.subagent_fields())
        .unwrap();
    let (id, purpose, concurrency, requests, timeout, output_tokens) = (
        &fields[0], &fields[1], &fields[2], &fields[4], &fields[5], &fields[6],
    );
    let set = |field: &gpui::Entity<gpui_elements::editable_text::EditableTextState>,
               text: &str,
               cx: &mut TestAppContext| {
        field.update(cx, |field, cx| field.emplace(text, cx));
        cx.run_until_parked();
    };
    let notice = |cx: &mut TestAppContext| {
        window
            .update(cx, |view, _, _| view.notice().cloned())
            .unwrap()
    };

    set(output_tokens, "64000", cx);
    assert_eq!(
        saved(&data).subagents.unwrap().max_output_tokens,
        Some(64_000)
    );
    set(output_tokens, "", cx);
    assert_eq!(
        saved(&data).subagents.unwrap().max_output_tokens,
        None,
        "empty uses the model's own limit"
    );
    // Request and time limits are optional too: none by default, empty removes them.
    assert_eq!(
        requests.read_with(cx, |field, _| field.as_str().to_owned()),
        ""
    );
    set(timeout, "600", cx);
    assert_eq!(saved(&data).subagents.unwrap().timeout_seconds, Some(600));
    set(timeout, "", cx);
    assert_eq!(saved(&data).subagents.unwrap().timeout_seconds, None);
    set(requests, "0", cx);
    assert!(notice(cx).is_some_and(|(text, _)| text.contains("or empty for no limit")));
    set(requests, "20", cx);
    assert_eq!(
        saved(&data).subagents.unwrap().max_model_requests_per_task,
        Some(20)
    );
    set(concurrency, "", cx);
    assert!(notice(cx).is_none(), "an emptied field is being retyped");
    set(concurrency, "99", cx);
    assert!(notice(cx).is_some_and(|(text, error)| error && text.contains("Parallel tasks")));
    assert_eq!(saved(&data).subagents.unwrap().max_concurrent, None);
    set(concurrency, "3", cx);
    assert!(notice(cx).is_none());
    assert_eq!(saved(&data).subagents.unwrap().max_concurrent, Some(3));

    // Editing an existing profile saves its purpose and keeps its (now unavailable) model.
    set(id, "explore", cx);
    set(purpose, "Find code and tests", cx);
    let mut visual = VisualTestContext::from_window(window.into(), cx);
    // Tall enough to show the form without scrolling.
    visual.simulate_resize(gpui::size(gpui::px(980.), gpui::px(1400.)));
    let save = visual.debug_bounds("save-subagent-profile").unwrap();
    visual.simulate_click(save.center(), Modifiers::none());
    cx.run_until_parked();
    let profile = saved(&data).subagents.unwrap().profiles["explore"].clone();
    assert_eq!(
        (profile.model.as_str(), profile.description.as_str()),
        ("gone/model", "Find code and tests")
    );
    assert!(
        id.read_with(cx, |id, _| id.as_str().is_empty()),
        "the form is cleared"
    );
}

/// A hand-edited settings file with a mistake is reported, not replaced with the settings
/// in memory (which would drop the user's edits).
#[gpui::test]
fn an_invalid_settings_file_is_not_overwritten(cx: &mut TestAppContext) {
    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let path = zp_core::paths::settings_file(data.path());
    let window = settings_window(Page::General, cx);
    let invalid = "{ \"notifications\": true, oops }";
    std::fs::write(&path, invalid).unwrap();
    let before = cx.update(|cx| AppSettings::get(cx).clone());
    window
        .update(cx, |view, _, cx| view.set_notifications(false, cx))
        .unwrap();
    let notice = window
        .update(cx, |view, _, _| view.notice().cloned())
        .unwrap();
    let (text, is_error) = notice.expect("the failure is reported");
    assert!(
        is_error && text.contains("settings.json") && text.contains("Fix it first"),
        "{text}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    assert_eq!(cx.update(|cx| AppSettings::get(cx).clone()), before);

    // Without a file, changes are saved as usual.
    std::fs::remove_file(&path).unwrap();
    window
        .update(cx, |view, _, cx| view.set_notifications(false, cx))
        .unwrap();
    assert_eq!(saved(&data).notifications, Some(false));
    assert_eq!(
        cx.update(|cx| AppSettings::get(cx).notifications),
        Some(false)
    );
}

/// A change that cannot be saved is not applied either, so the app never runs with
/// settings the file does not have.
#[cfg(unix)]
#[gpui::test]
fn settings_that_cannot_be_saved_are_not_applied(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let set_mode = |mode| {
        std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o555);
    if std::fs::write(data.path().join("probe"), "").is_ok() {
        // Running as root: permissions do not stop writes.
        set_mode(0o755);
        return;
    }
    let result = cx.update(|cx| AppSettings::update(cx, |s| s.notifications = Some(false)));
    set_mode(0o755);
    assert!(result.is_err());
    assert_eq!(cx.update(|cx| AppSettings::get(cx).notifications), None);
    assert_eq!(saved(&data).notifications, None);
}

/// A keychain whose reads can be denied, counting them.
#[derive(Default)]
struct DeniedReads {
    deny: std::sync::atomic::AtomicBool,
    reads: std::sync::atomic::AtomicUsize,
    inner: zp_core::credentials::MemoryBackend,
}

impl zp_core::credentials::SecretBackend for DeniedReads {
    fn get(&self, provider: &str) -> anyhow::Result<Option<String>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(
            !self.deny.load(std::sync::atomic::Ordering::SeqCst),
            "keychain access denied"
        );
        self.inner.get(provider)
    }
    fn set(&self, provider: &str, secret: &str) -> anyhow::Result<()> {
        self.inner.set(provider, secret)
    }
    fn delete(&self, provider: &str) -> anyhow::Result<()> {
        self.inner.delete(provider)
    }
}

/// A keychain read that failed is not retried on its own, only when the user opens the
/// Providers page.
#[gpui::test]
fn opening_the_providers_page_retries_failed_keychain_reads(cx: &mut TestAppContext) {
    use zp_ai::CredentialStore as _;
    use zp_core::credentials::Credentials;

    let faux = FauxProvider::default();
    let data = setup_models(cx, &[&faux], false);
    let keychain = Arc::new(DeniedReads::default());
    let index = data.path().join("credentials.json");
    Credentials::new(keychain.clone(), Some(index.clone()))
        .set("deepseek", "sk-1")
        .unwrap();
    let credentials = Arc::new(Credentials::new(keychain.clone(), Some(index)));
    cx.update(|cx| {
        let app = cx.global_mut::<AppModels>();
        app.models = app.models.clone().with_credentials(credentials.clone());
        app.credentials = credentials.clone();
    });
    keychain
        .deny
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(credentials.api_key("deepseek"), None);
    keychain
        .deny
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(credentials.api_key("deepseek"), None);

    let window = settings_window(Page::General, cx);
    assert_eq!(credentials.api_key("deepseek"), None);
    assert_eq!(keychain.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    window
        .update(cx, |view, _, cx| view.set_page(Page::Providers, cx))
        .unwrap();
    assert_eq!(credentials.api_key("deepseek").as_deref(), Some("sk-1"));
}
