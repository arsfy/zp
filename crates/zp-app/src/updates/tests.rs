use gpui::TestAppContext;
use zp_ai::FauxProvider;
use zp_core::update::Asset;

use super::*;
use crate::tests_conversation::{active, open, setup_models, wait_until};

fn available(cx: &mut TestAppContext) -> Update {
    let update = Update {
        version: "99.0.0-rc.10".into(),
        channel: UpdateChannel::Preview,
        notes: "Changes".into(),
        release_url: format!("{}/releases", zp_core::update::REPOSITORY_URL),
        asset: Asset {
            name: "ZP.zip".into(),
            kind: PackageKind::MacApp,
            url: "http://127.0.0.1:1/ZP.zip".into(),
            size: 3,
            sha256: "00".repeat(32),
        },
    };
    cx.update(|cx| set_state(UpdateState::Available(update.clone()), cx));
    update
}

#[gpui::test]
fn update_waits_for_confirmation_and_later_preserves_the_offer(cx: &mut TestAppContext) {
    let data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_directory, project) = crate::tests::project_dir();
    let _window = open(cx, &project);
    let update = available(cx);
    cx.update(activate);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    assert_eq!(
        cx.update(|cx| state(cx)),
        UpdateState::Available(update.clone())
    );
    assert!(!data.path().join("updates").exists());
    // A check while the dialog is up would void its answer: it waits.
    cx.update(|cx| check(false, cx));
    assert_eq!(
        cx.update(|cx| state(cx)),
        UpdateState::Available(update.clone())
    );
    cx.simulate_prompt_answer("Later");
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| state(cx)), UpdateState::Available(update));
    assert!(!data.path().join("updates").exists());
}

#[gpui::test]
fn an_old_dialog_cannot_install_after_channel_changes(cx: &mut TestAppContext) {
    let data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_directory, project) = crate::tests::project_dir();
    let _window = open(cx, &project);
    available(cx);
    cx.update(activate);
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.update(|cx| {
        AppSettings::update(cx, |settings| {
            settings.update_feed = Some("http://127.0.0.1:1/releases".into())
        })
        .unwrap();
        set_channel(UpdateChannel::Stable, cx).unwrap();
    });
    cx.simulate_prompt_answer("Update and Restart");
    cx.run_until_parked();
    assert!(!matches!(
        cx.update(|cx| state(cx)),
        UpdateState::Downloading { .. } | UpdateState::Installing
    ));
    assert!(!data.path().join("updates").exists());
    assert_eq!(
        zp_core::settings::Settings::load(&zp_core::paths::settings_file(data.path()))
            .unwrap()
            .update_channel,
        Some(UpdateChannel::Stable)
    );
}

#[gpui::test]
fn duplicate_confirmation_and_cancel_do_not_start_another_download(cx: &mut TestAppContext) {
    let data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_directory, project) = crate::tests::project_dir();
    let _window = open(cx, &project);
    let update = available(cx);
    cx.update(activate);
    cx.run_until_parked();
    cx.update(activate);
    cx.simulate_prompt_answer("Update and Restart");
    wait_until(cx, |cx| {
        matches!(cx.update(|cx| state(cx)), UpdateState::Failed(_))
    });
    // The loopback package is fetched (and the connection refused), not rejected as HTTP.
    assert!(
        matches!(cx.update(|cx| state(cx)), UpdateState::Failed(error) if !error.contains("HTTPS"))
    );
    assert!(!data.path().join("updates/ZP.zip").exists());
    cx.update(|cx| {
        set_state(
            UpdateState::Downloading {
                update: update.clone(),
                received: 1,
                total: 3,
            },
            cx,
        );
        let token = CancellationToken::new();
        cx.global_mut::<AppUpdate>().cancel = Some(token.clone());
        cancel(cx);
        assert!(token.is_cancelled());
        assert_eq!(state(cx), UpdateState::Available(update));
    });
}

#[gpui::test]
fn update_restart_preserves_drafts_and_reports_save_failure(cx: &mut TestAppContext) {
    let data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_directory, project) = crate::tests::project_dir();
    let window = open(cx, &project);
    let first = active(window, cx);
    cx.update(|cx| first.update(cx, |view, cx| view.set_draft("first unsent draft", cx)));
    cx.dispatch_action(window.into(), crate::actions::NewChat);
    cx.run_until_parked();
    let second = active(window, cx);
    cx.update(|cx| second.update(cx, |view, cx| view.set_draft("second unsent draft", cx)));
    cx.update(crate::workspace::save_for_update).unwrap();
    let saved = zp_core::app_state::AppState::load(&zp_core::paths::state_file(data.path()));
    assert_eq!(
        saved
            .project(&project)
            .tabs
            .iter()
            .map(|tab| tab.draft.as_str())
            .collect::<Vec<_>>(),
        ["first unsent draft", "second unsent draft"]
    );
    drop(first);
    drop(second);
    window
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let reopened = open(cx, &project);
    let drafts = reopened
        .update(cx, |view, _, cx| {
            view.tabs()
                .iter()
                .map(|tab| tab.conversation.read(cx).draft(cx))
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(drafts, ["first unsent draft", "second unsent draft"]);
    let path = zp_core::paths::state_file(data.path());
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(cx.update(crate::workspace::save_for_update).is_err());
    assert!(reopened.update(cx, |_, _, _| ()).is_ok());
}

#[gpui::test]
fn confirmed_update_downloads_verifies_and_reaches_installation(cx: &mut TestAppContext) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_directory, project) = crate::tests::project_dir();
    let _window = open(cx, &project);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();
    runtime.spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 3\r\n\r\nzp!")
                .await;
        }
    });
    let mut update = available(cx);
    update.asset.url = format!("http://{address}/ZP.zip");
    update.asset.sha256 = zp_core::update::sha256_hex(b"zp!");
    // Outside an AppImage, installing one stops before touching any file.
    update.asset.kind = PackageKind::AppImage;
    cx.update(|cx| set_state(UpdateState::Available(update.clone()), cx));
    cx.update(activate);
    cx.run_until_parked();
    cx.simulate_prompt_answer("Update and Restart");
    // Downloaded and verified, then installation starts: the test runs from no AppImage,
    // so preparing it fails (and the update can be retried).
    wait_until(cx, |cx| {
        matches!(cx.update(|cx| state(cx)), UpdateState::Failed(_))
    });
    let downloaded = data.path().join("updates/ZP.zip");
    assert_eq!(std::fs::read(&downloaded).unwrap(), b"zp!");
    assert!(
        matches!(cx.update(|cx| state(cx)), UpdateState::Failed(error) if error.contains("AppImage"))
    );
    assert!(cx.update(|cx| retry_available(cx)));
    if cx.has_pending_prompt() {
        cx.simulate_prompt_answer("OK");
    }
    drop(runtime);
}

#[gpui::test]
fn update_dialog_answers_with_keys_and_buttons(cx: &mut TestAppContext) {
    let _data = setup_models(cx, &[&FauxProvider::default()], false);
    let (_directory, project) = crate::tests::project_dir();
    let window = open(cx, &project);
    let ask = |cx: &mut TestAppContext| {
        let answer = window
            .update(cx, |_, window, cx| {
                cx.set_prompt_builder(dialog::build);
                let answer = window.prompt(
                    PromptLevel::Info,
                    "Update ZP",
                    Some("0.1.0 → 0.2.0\n\n• Notes"),
                    &["Update and Restart", "Later", "Release Notes"],
                    cx,
                );
                cx.reset_prompt_builder();
                answer
            })
            .unwrap();
        cx.run_until_parked();
        answer
    };
    let mut answer = ask(cx);
    cx.simulate_keystrokes(window.into(), "escape");
    cx.run_until_parked();
    assert_eq!(answer.try_recv().unwrap(), Some(1));
    let mut answer = ask(cx);
    cx.simulate_keystrokes(window.into(), "enter");
    cx.run_until_parked();
    assert_eq!(answer.try_recv().unwrap(), Some(0));
}

#[test]
fn release_notes_become_a_plain_excerpt() {
    let notes = "## What's Changed\n* Faster search by @someone in https://github.com/arsfy/zp/pull/7\n- Signed manifests\n\n**Full Changelog**: https://github.com/arsfy/zp/compare/v0.1.0...v0.2.0";
    assert_eq!(
        notes_excerpt(notes, 8).unwrap(),
        "• Faster search\n• Signed manifests"
    );
    assert_eq!(notes_excerpt(notes, 1).unwrap(), "• Faster search");
    assert!(notes_excerpt("## Heading only\n", 8).is_none());
}
