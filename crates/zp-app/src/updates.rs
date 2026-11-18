//! One updater shared by title bars, About and the application menu.

mod dialog;
mod install;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{App, Global, IntoElement, MouseButton, PromptLevel, div, prelude::*, px};
use tokio_util::sync::CancellationToken;
use zp_core::update::{CheckResult, PackageKind, Update, UpdateChannel};

use crate::store::{AppPaths, AppSettings};
use crate::theme::ActiveTheme;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Unavailable(String),
    Available(Update),
    Downloading {
        update: Update,
        received: u64,
        total: u64,
    },
    Ready(Update, PathBuf),
    Installing,
    Failed(String),
}

#[derive(Default)]
pub struct AppUpdate {
    pub state: UpdateState,
    pub last_checked: Option<chrono::DateTime<chrono::Local>>,
    generation: u64,
    cancel: Option<CancellationToken>,
    dialog_open: bool,
    retry: Option<Update>,
}

impl Global for AppUpdate {}

pub fn init(cx: &mut App) {
    cx.set_global(AppUpdate::default());
    cx.on_action(|_: &crate::actions::CheckForUpdates, cx| check(true, cx));
}

pub fn state(cx: &App) -> UpdateState {
    cx.global::<AppUpdate>().state.clone()
}

fn set_state(state: UpdateState, cx: &mut App) {
    cx.global_mut::<AppUpdate>().state = state;
    cx.refresh_windows();
}

pub fn channel(cx: &App) -> UpdateChannel {
    AppSettings::get(cx).update_channel.unwrap_or_default()
}

pub fn set_channel(selected: UpdateChannel, cx: &mut App) -> anyhow::Result<()> {
    anyhow::ensure!(
        !matches!(state(cx), UpdateState::Installing),
        "an update is being installed"
    );
    AppSettings::update(cx, |s| s.update_channel = Some(selected))?;
    cancel(cx);
    set_state(UpdateState::Idle, cx);
    check(false, cx);
    Ok(())
}

pub fn feed_url(cx: &App) -> String {
    std::env::var("ZP_UPDATE_FEED")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| AppSettings::get(cx).update_feed.clone())
        .unwrap_or_else(|| zp_core::update::DEFAULT_RELEASES_URL.to_owned())
}

pub fn check_on_startup(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(5)).await;
        loop {
            cx.update(|cx| {
                if AppSettings::get(cx).check_for_updates.unwrap_or(true) {
                    check(false, cx);
                }
            });
            cx.background_executor()
                .timer(Duration::from_secs(6 * 60 * 60))
                .await;
        }
    })
    .detach();
}

pub fn check(manual: bool, cx: &mut App) {
    match state(cx) {
        UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Installing => return,
        UpdateState::Ready(..) => {
            if manual {
                activate(cx);
            }
            return;
        }
        _ => {}
    }
    // A new check would void the answer of the update dialog on screen.
    if cx.global::<AppUpdate>().dialog_open {
        return;
    }
    cancel(cx);
    cx.global_mut::<AppUpdate>().retry = None;
    let generation = cx.global::<AppUpdate>().generation;
    let cancel = CancellationToken::new();
    cx.global_mut::<AppUpdate>().cancel = Some(cancel.clone());
    set_state(UpdateState::Checking, cx);
    let url = feed_url(cx);
    let selected = channel(cx);
    let task = gpui_tokio::Tokio::spawn(cx, async move {
        let platform = zp_core::update::platform_key();
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("update check cancelled"),
            result = zp_core::update::check(&url, selected, zp_core::VERSION,
                &platform, PackageKind::current(), zp_core::update::PUBLIC_KEY) => result,
        }
    });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            if cx.global::<AppUpdate>().generation != generation {
                return;
            }
            let state = match result {
                Ok(Ok(CheckResult::Available(update))) => UpdateState::Available(update),
                Ok(Ok(CheckResult::UpToDate)) => UpdateState::UpToDate,
                Ok(Ok(CheckResult::Unavailable(version))) => UpdateState::Unavailable(version),
                Ok(Err(error)) => UpdateState::Failed(format!("{error:#}")),
                Err(error) => UpdateState::Failed(error.to_string()),
            };
            let updater = cx.global_mut::<AppUpdate>();
            updater.cancel = None;
            updater.last_checked = Some(chrono::Local::now());
            set_state(state.clone(), cx);
            if manual {
                match state {
                    UpdateState::Available(update) => offer(&update, cx),
                    UpdateState::UpToDate => {
                        prompt(
                            PromptLevel::Info,
                            "ZP is up to date",
                            &format!("Version {} · {}", zp_core::VERSION, selected.label()),
                            &["OK"],
                            cx,
                        );
                    }
                    UpdateState::Unavailable(version) => {
                        prompt(
                            PromptLevel::Info,
                            "No compatible update package",
                            &format!("ZP {version} has no package for this installation."),
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
            // Update dialogs show the ZP logo; tests answer the platform's test prompts.
            #[cfg(not(test))]
            cx.set_prompt_builder(dialog::build);
            let answer = window.prompt(level, title, Some(detail), answers, cx);
            #[cfg(not(test))]
            cx.reset_prompt_builder();
            answer
        })
        .ok()
}

pub fn offer(update: &Update, cx: &mut App) {
    if cx.global::<AppUpdate>().dialog_open || state(cx) != UpdateState::Available(update.clone()) {
        return;
    }
    let mut detail = format!(
        "{} → {} · {} · {:.1} MB",
        zp_core::VERSION,
        update.version,
        update.channel.label(),
        update.asset.size as f64 / 1_000_000.0,
    );
    if let Some(notes) = notes_excerpt(&update.notes, 8) {
        detail.push_str("\n\n");
        detail.push_str(&notes);
    }
    let Some(answer) = prompt(
        PromptLevel::Info,
        "Update ZP",
        &detail,
        &["Update and Restart", "Later", "Release Notes"],
        cx,
    ) else {
        return;
    };
    cx.global_mut::<AppUpdate>().dialog_open = true;
    let generation = cx.global::<AppUpdate>().generation;
    let update = update.clone();
    cx.spawn(async move |cx| {
        let answer = answer.await;
        cx.update(|cx| {
            cx.global_mut::<AppUpdate>().dialog_open = false;
            if cx.global::<AppUpdate>().generation != generation {
                return;
            }
            if answer == Ok(0) && state(cx) == UpdateState::Available(update.clone()) {
                download(update, cx);
            } else if answer == Ok(2) {
                cx.open_url(&update.release_url);
            }
        });
    })
    .detach();
}

/// The first `lines` items of release notes as plain text: GitHub's generated Markdown
/// without headings, links to pull requests or the changelog footer.
pub fn notes_excerpt(notes: &str, lines: usize) -> Option<String> {
    static PULL_REQUEST: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pull_request =
        PULL_REQUEST.get_or_init(|| regex::Regex::new(r"\s+by @\S+ in https://\S+$").unwrap());
    let excerpt: Vec<String> = notes
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && !line.starts_with('#') && !line.starts_with("**Full Changelog**")
        })
        .map(|line| {
            let line = pull_request.replace(line, "");
            match line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")) {
                Some(item) => format!("• {item}"),
                None => line.into_owned(),
            }
        })
        .take(lines)
        .collect();
    let excerpt = excerpt.join("\n");
    if excerpt.is_empty() {
        return None;
    }
    let mut short: String = excerpt.chars().take(600).collect();
    if short.len() < excerpt.len() {
        short.push('…');
    }
    Some(short)
}

pub fn cancel(cx: &mut App) {
    let updater = cx.global_mut::<AppUpdate>();
    updater.generation += 1;
    if let Some(cancel) = updater.cancel.take() {
        cancel.cancel();
    }
    if let UpdateState::Downloading { update, .. } = &updater.state {
        updater.state = UpdateState::Available(update.clone());
    }
    cx.refresh_windows();
}

enum DownloadEvent {
    Progress(u64, u64),
    Finished(anyhow::Result<PathBuf>),
}

fn download(update: Update, cx: &mut App) {
    cx.global_mut::<AppUpdate>().retry = Some(update.clone());
    let Some(dir) = cx
        .global::<AppPaths>()
        .data_dir
        .clone()
        .map(|d| d.join("updates"))
    else {
        set_state(
            UpdateState::Failed("No update download directory is available.".into()),
            cx,
        );
        return;
    };
    cancel(cx);
    let generation = cx.global::<AppUpdate>().generation;
    let cancel = CancellationToken::new();
    cx.global_mut::<AppUpdate>().cancel = Some(cancel.clone());
    set_state(
        UpdateState::Downloading {
            update: update.clone(),
            received: 0,
            total: update.asset.size,
        },
        cx,
    );
    let (sender, mut events) = futures::channel::mpsc::unbounded();
    let asset = update.asset.clone();
    let task = gpui_tokio::Tokio::spawn(cx, async move {
        // Coalesce progress so fast downloads cannot flood the UI event queue.
        let progress_sender = sender.clone();
        let percent = std::sync::atomic::AtomicU64::new(101);
        let result = zp_core::update::download(&asset, &dir, &cancel, move |received, total| {
            let next = received.saturating_mul(100) / total;
            if percent.swap(next, std::sync::atomic::Ordering::Relaxed) != next {
                let _ = progress_sender.unbounded_send(DownloadEvent::Progress(received, total));
            }
        })
        .await;
        let _ = sender.unbounded_send(DownloadEvent::Finished(result));
    });
    cx.spawn(async move |cx| {
        while let Some(event) = events.next().await {
            let current = cx.update(|cx| {
                if cx.global::<AppUpdate>().generation != generation {
                    return false;
                }
                match event {
                    DownloadEvent::Progress(received, total) => set_state(
                        UpdateState::Downloading {
                            update: update.clone(),
                            received,
                            total,
                        },
                        cx,
                    ),
                    DownloadEvent::Finished(Ok(path)) => {
                        cx.global_mut::<AppUpdate>().cancel = None;
                        set_state(UpdateState::Ready(update.clone(), path), cx);
                        activate(cx);
                    }
                    DownloadEvent::Finished(Err(error)) => {
                        fail("Could not download the update", error, cx)
                    }
                }
                true
            });
            if !current {
                break;
            }
        }
        let _ = task.await;
    })
    .detach();
}

pub fn activate(cx: &mut App) {
    match state(cx) {
        UpdateState::Available(update) => offer(&update, cx),
        UpdateState::Ready(update, path) => {
            if cx.global::<AppUpdate>().dialog_open {
                return;
            }
            apply(update, path, cx);
        }
        UpdateState::Failed(_) => {
            if let Some(update) = cx.global::<AppUpdate>().retry.clone() {
                set_state(UpdateState::Available(update.clone()), cx);
                offer(&update, cx);
            }
        }
        _ => {}
    }
}

fn apply(update: Update, path: PathBuf, cx: &mut App) {
    if matches!(
        update.asset.kind,
        PackageKind::Deb | PackageKind::WindowsInstaller
    ) {
        if let Err(error) = zp_core::update::verify_download(&path, &update.asset) {
            fail("Could not install the update", error, cx);
            return;
        }
        if let Err(error) = crate::workspace::save_for_update(cx) {
            fail("Could not save your workspace", error, cx);
            return;
        }
        cx.open_with_system(&path);
        let deb = update.asset.kind == PackageKind::Deb;
        set_state(UpdateState::Ready(update, path), cx);
        prompt(
            PromptLevel::Info,
            if deb {
                "Install with your package manager"
            } else {
                "Install the update"
            },
            "Finish installing the downloaded package, then restart ZP.",
            &["OK"],
            cx,
        );
        return;
    }
    let bundle = (update.asset.kind == PackageKind::MacApp)
        .then(|| cx.app_path().ok())
        .flatten();
    let ready = (update.clone(), path.clone());
    set_state(UpdateState::Installing, cx);
    let task = gpui_tokio::Tokio::spawn(cx, async move {
        tokio::task::spawn_blocking(move || install::prepare(&update, &path, bundle.as_deref()))
            .await?
    });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| match result {
            Ok(Ok(prepared)) => {
                finish_install(prepared, ready, cx);
            }
            Ok(Err(error)) => fail("Could not install the update", error, cx),
            Err(error) => fail(
                "Could not install the update",
                anyhow::anyhow!(error.to_string()),
                cx,
            ),
        });
    })
    .detach();
}

fn finish_install(prepared: install::Prepared, ready: (Update, PathBuf), cx: &mut App) {
    if crate::workspace::has_update_interruptions(cx) {
        let Some(answer) = prompt(
            PromptLevel::Warning,
            "Restart to update ZP?",
            "Running tasks, drafts or terminals are open. Restarting stops running tasks; your workspace and drafts will be restored.",
            &["Install and Restart", "Later"],
            cx,
        ) else {
            set_state(UpdateState::Ready(ready.0, ready.1), cx);
            return;
        };
        cx.global_mut::<AppUpdate>().dialog_open = true;
        cx.spawn(async move |cx| {
            let answer = answer.await;
            cx.update(|cx| {
                cx.global_mut::<AppUpdate>().dialog_open = false;
                if answer == Ok(0) {
                    start_prepared(prepared, cx);
                } else {
                    set_state(UpdateState::Ready(ready.0, ready.1), cx);
                }
            });
        })
        .detach();
    } else {
        start_prepared(prepared, cx);
    }
}

fn start_prepared(prepared: install::Prepared, cx: &mut App) {
    if let Err(error) = crate::workspace::save_for_update(cx) {
        fail("Could not save your workspace", error, cx);
        return;
    }
    match prepared.start() {
        Ok(()) => crate::workspace::quit(cx),
        Err(error) => fail("Could not install the update", error, cx),
    }
}

pub fn retry_available(cx: &App) -> bool {
    matches!(state(cx), UpdateState::Failed(_)) && cx.global::<AppUpdate>().retry.is_some()
}

fn fail(title: &str, error: anyhow::Error, cx: &mut App) {
    let detail = format!("{error:#}");
    cx.global_mut::<AppUpdate>().cancel = None;
    set_state(UpdateState::Failed(detail.clone()), cx);
    prompt(PromptLevel::Warning, title, &detail, &["OK"], cx);
}

/// Set by the update helper on the new version: a file to create once it has started.
const ACK_VARIABLE: &str = "ZP_UPDATE_ACK";
/// Set by the update helper when it restored the previous version: its log directory.
const ROLLBACK_VARIABLE: &str = "ZP_UPDATE_ROLLBACK";

/// What the update helper passed to this process.
struct Handoff {
    ack: Option<std::ffi::OsString>,
    rollback: Option<std::ffi::OsString>,
}

static HANDOFF: std::sync::OnceLock<Handoff> = std::sync::OnceLock::new();

/// Takes the update helper's variables out of the environment, so programs started from
/// ZP (terminals, tools, another ZP) don't inherit them. Call first in `main`, before any
/// other thread starts.
#[allow(unsafe_code)]
pub fn take_handoff() {
    let handoff = Handoff {
        ack: std::env::var_os(ACK_VARIABLE),
        rollback: std::env::var_os(ROLLBACK_VARIABLE),
    };
    for name in [ACK_VARIABLE, ROLLBACK_VARIABLE] {
        if std::env::var_os(name).is_some() {
            // SAFETY: runs at the start of `main`, before any other thread exists.
            unsafe { std::env::remove_var(name) };
        }
    }
    let _ = HANDOFF.set(handoff);
}

pub fn acknowledge_startup(cx: &mut App) {
    let Some(handoff) = HANDOFF.get() else {
        return;
    };
    if let Some(path) = &handoff.ack
        && let Err(error) = std::fs::write(path, b"ok")
    {
        tracing::warn!(%error, "could not acknowledge update startup");
    }
    if let Some(stage) = &handoff.rollback {
        prompt(
            PromptLevel::Warning,
            "The update could not start",
            &format!(
                "Your previous version of ZP has been restored. Update logs: {}",
                PathBuf::from(stage).display()
            ),
            &["OK"],
            cx,
        );
    }
}

/// The Update button of title rows (`compact` in the status bar): shown while an update
/// is available, downloading, installing or can be retried.
pub fn title_bar_button(compact: bool, cx: &App) -> Option<impl IntoElement> {
    let label = match state(cx) {
        UpdateState::Available(_) | UpdateState::Ready(..) => "Update".to_owned(),
        UpdateState::Downloading {
            received, total, ..
        } => format!("{}%", received.saturating_mul(100) / total.max(1)),
        UpdateState::Installing => "Installing…".to_owned(),
        UpdateState::Failed(_) if retry_available(cx) => "Retry Update".to_owned(),
        _ => return None,
    };
    let downloading = matches!(state(cx), UpdateState::Downloading { .. });
    let colors = cx.theme().colors.clone();
    Some(
        div()
            .id("title-update")
            .h(px(if compact { 20. } else { 22. }))
            .min_w(px(if compact { 48. } else { 64. }))
            .px_2()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_size(px(if compact { 11. } else { 12. }))
            .bg(colors.accent)
            .text_color(colors.accent_text)
            .cursor_pointer()
            .child(label)
            .tooltip(crate::ui::tooltip(if downloading {
                "Cancel update download"
            } else {
                "Update ZP"
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                if downloading {
                    cancel(cx);
                } else {
                    activate(cx);
                }
            }),
    )
}
