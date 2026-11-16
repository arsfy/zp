//! Update checks in the app: at startup (unless turned off) and from "Check for
//! Updates…". An available update shows in the status bar; installing downloads it,
//! verifies its SHA-256, and on macOS replaces the app bundle and relaunches (elsewhere
//! the downloaded installer is opened).

use std::path::{Path, PathBuf};

use gpui::{App, Global, PromptLevel};
use zp_core::update::Update;

use crate::store::{AppPaths, AppSettings};

/// Where an update stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Update),
    Downloading(Update),
    Failed(String),
}

#[derive(Default)]
pub struct AppUpdate {
    pub state: UpdateState,
}

impl Global for AppUpdate {}

pub fn init(cx: &mut App) {
    cx.set_global(AppUpdate::default());
    cx.on_action(|_: &crate::actions::CheckForUpdates, cx| check(true, cx));
}

pub fn state(cx: &App) -> UpdateState {
    cx.try_global::<AppUpdate>()
        .map(|u| u.state.clone())
        .unwrap_or_default()
}

fn set_state(state: UpdateState, cx: &mut App) {
    cx.default_global::<AppUpdate>().state = state;
    cx.refresh_windows();
}

/// The feed URL: `ZP_UPDATE_FEED`, the settings, or the release feed.
pub fn feed_url(cx: &App) -> String {
    std::env::var("ZP_UPDATE_FEED")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| AppSettings::get(cx).update_feed.clone())
        .unwrap_or_else(|| zp_core::update::DEFAULT_FEED_URL.to_owned())
}

/// The check at startup, unless turned off in the settings.
pub fn check_on_startup(cx: &mut App) {
    if AppSettings::get(cx).check_for_updates.unwrap_or(true) {
        check(false, cx);
    }
}

/// Looks for an update; `manual` checks report "up to date" and failures in a dialog.
pub fn check(manual: bool, cx: &mut App) {
    if matches!(
        state(cx),
        UpdateState::Checking | UpdateState::Downloading(_)
    ) {
        return;
    }
    set_state(UpdateState::Checking, cx);
    let url = feed_url(cx);
    let task = gpui_tokio::Tokio::spawn(cx, async move { zp_core::update::check(&url).await });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            let state = match result {
                Ok(Ok(Some(update))) => UpdateState::Available(update),
                Ok(Ok(None)) => UpdateState::UpToDate,
                Ok(Err(error)) => UpdateState::Failed(format!("{error:#}")),
                Err(error) => UpdateState::Failed(error.to_string()),
            };
            tracing::info!(?state, "update check");
            set_state(state.clone(), cx);
            if manual {
                match state {
                    UpdateState::Available(update) => offer(&update, cx),
                    UpdateState::UpToDate => {
                        prompt(
                            PromptLevel::Info,
                            "ZP is up to date",
                            &format!("Version {}", zp_core::VERSION),
                            &["OK"],
                            cx,
                        );
                    }
                    UpdateState::Failed(error) => {
                        prompt(
                            PromptLevel::Warning,
                            "Could not check for updates",
                            &error,
                            &["OK"],
                            cx,
                        );
                    }
                    _ => {}
                }
            }
        });
    })
    .detach();
}

/// Shows a dialog in the active (or any) window; returns whether one was shown.
fn prompt(
    level: PromptLevel,
    title: &str,
    detail: &str,
    answers: &[&str],
    cx: &mut App,
) -> Option<futures::channel::oneshot::Receiver<usize>> {
    let window = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())?;
    window
        .update(cx, |_, window, cx| {
            window.prompt(level, title, Some(detail), answers, cx)
        })
        .ok()
}

/// Asks whether to install `update`.
pub fn offer(update: &Update, cx: &mut App) {
    let detail = format!(
        "ZP {} is available (you have {}).{}",
        update.version,
        zp_core::VERSION,
        update
            .notes
            .as_deref()
            .map(|notes| format!("\n\n{}", notes.trim()))
            .unwrap_or_default()
    );
    let Some(answer) = prompt(
        PromptLevel::Info,
        "Update available",
        &detail,
        &["Install and Restart", "Later"],
        cx,
    ) else {
        return;
    };
    let update = update.clone();
    cx.spawn(async move |cx| {
        if answer.await == Ok(0) {
            cx.update(|cx| install(update, cx));
        }
    })
    .detach();
}

/// Downloads, verifies and installs `update`.
pub fn install(update: Update, cx: &mut App) {
    let Some(dir) = cx
        .try_global::<AppPaths>()
        .and_then(|p| p.data_dir.clone())
        .map(|d| d.join("updates"))
    else {
        return;
    };
    set_state(UpdateState::Downloading(update.clone()), cx);
    let asset = update.asset.clone();
    let task =
        gpui_tokio::Tokio::spawn(
            cx,
            async move { zp_core::update::download(&asset, &dir).await },
        );
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| match result {
            Ok(Ok(path)) => {
                if let Err(error) = apply(&path, cx) {
                    set_state(UpdateState::Failed(format!("{error:#}")), cx);
                    prompt(
                        PromptLevel::Warning,
                        "Could not install the update",
                        &format!("{error:#}"),
                        &["OK"],
                        cx,
                    );
                }
            }
            Ok(Err(error)) => {
                set_state(UpdateState::Failed(format!("{error:#}")), cx);
                prompt(
                    PromptLevel::Warning,
                    "Could not download the update",
                    &format!("{error:#}"),
                    &["OK"],
                    cx,
                );
            }
            Err(error) => set_state(UpdateState::Failed(error.to_string()), cx),
        });
    })
    .detach();
}

/// Installs a downloaded build.
fn apply(download: &Path, cx: &mut App) -> anyhow::Result<()> {
    if cfg!(target_os = "macos")
        && download.extension().is_some_and(|e| e == "zip")
        && let Ok(bundle) = cx.app_path()
    {
        let staging = download.with_extension("staged");
        let _ = std::fs::remove_dir_all(&staging);
        let status = std::process::Command::new("ditto")
            .args(["-x", "-k"])
            .arg(download)
            .arg(&staging)
            .status()?;
        anyhow::ensure!(status.success(), "could not unpack the update");
        let new_app: PathBuf = std::fs::read_dir(&staging)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or_else(|| anyhow::anyhow!("the update contains no app"))?;
        // Swap the bundle once this process has exited, then relaunch.
        let script = r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; rm -rf "$2" && mv "$3" "$2" && open "$2""#;
        std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("zp-update")
            .arg(std::process::id().to_string())
            .arg(&bundle)
            .arg(&new_app)
            .spawn()?;
        crate::workspace::quit(cx);
        return Ok(());
    }
    // Installers, AppImages and packages are opened with the system.
    cx.open_with_system(download);
    prompt(
        PromptLevel::Info,
        "Update downloaded",
        &format!(
            "Finish installing with {}, then restart ZP.",
            download.display()
        ),
        &["OK"],
        cx,
    );
    Ok(())
}
