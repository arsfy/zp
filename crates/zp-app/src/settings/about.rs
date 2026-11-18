//! Settings › About: the application, its software updates and its repository.

use gpui::{ClipboardItem, Rgba, img, svg};
use zp_core::update::{REPOSITORY_URL, UpdateChannel};

use super::*;
use crate::updates::{self, AppUpdate, UpdateState};

/// Left inset of text that sits next to the status dot.
const DOT_INSET: f32 = 20.0;

const ICON_COPY: &[u8] = include_bytes!("../../../../assets/icons/copy.svg");
const ICON_CHECK: &[u8] = include_bytes!("../../../../assets/icons/check.svg");
const ICON_GITHUB: &[u8] = include_bytes!("../../../../assets/icons/github.svg");
const ICON_EXTERNAL: &[u8] =
    include_bytes!("../../../../assets/icons/square-arrow-out-up-right.svg");
const ICON_REFRESH: &[u8] = include_bytes!("../../../../assets/icons/rotate-cw.svg");

impl SettingsWindow {
    pub fn set_update_channel(&mut self, channel: UpdateChannel, cx: &mut Context<Self>) {
        let result = updates::set_channel(channel, cx);
        self.report(result, cx);
    }

    pub fn set_auto_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |s| s.check_for_updates = Some(on));
        self.report(result, cx);
    }

    fn copy_version_info(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(version_info()));
        self.version_copied = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.version_copied = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_about(&self, cx: &mut Context<Self>) -> AnyElement {
        page_body()
            .gap_0()
            .pt_2()
            .child(self.render_identity(cx))
            .child(
                div()
                    .pt_6()
                    .pb_1()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Software Update"),
            )
            .child(self.render_status(cx))
            .child(self.render_channel(cx))
            .child(self.render_auto_check(cx))
            .into_any_element()
    }

    /// The logo, name, version and platform, with the repository link on the right.
    fn render_identity(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let platform = match build_commit() {
            Some(commit) => format!("{} · {commit}", zp_core::update::platform_key()),
            None => zp_core::update::platform_key(),
        };
        div()
            .flex()
            .items_center()
            .gap_4()
            .child(
                div()
                    .size(px(64.))
                    .flex_shrink_0()
                    .children(crate::ui::app_logo(cx).map(|logo| img(logo).size(px(64.)))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(zp_core::APP_NAME),
                            )
                            .child(pill(
                                format!("v{}", zp_core::VERSION),
                                colors.surface_hover,
                                colors.text,
                            ))
                            .child(pill(
                                UpdateChannel::default().label().to_owned(),
                                colors.accent,
                                colors.accent_text,
                            )),
                    )
                    .child(
                        div()
                            .text_color(colors.text_muted)
                            .child("A lightweight, native and high performance GUI Harness"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(platform)
                            .child(
                                icon_button(
                                    "copy-version",
                                    if self.version_copied.is_some() {
                                        ICON_CHECK
                                    } else {
                                        ICON_COPY
                                    },
                                    cx,
                                )
                                .tooltip(crate::ui::tooltip(if self.version_copied.is_some() {
                                    "Copied"
                                } else {
                                    "Copy version info"
                                }))
                                .on_click(cx.listener(|this, _, _, cx| this.copy_version_info(cx))),
                            ),
                    ),
            )
            .child(
                div().flex_shrink_0().child(
                    div()
                        .id("about-github")
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .font_weight(FontWeight::MEDIUM)
                        .hover_style(move |s| s.bg(colors.surface_hover))
                        .tooltip(crate::ui::tooltip(REPOSITORY_URL))
                        .child(
                            svg()
                                .data(ICON_GITHUB)
                                .size(px(16.))
                                .text_color(colors.text),
                        )
                        .child("GitHub")
                        .child(
                            svg()
                                .data(ICON_EXTERNAL)
                                .size(px(11.))
                                .text_color(colors.text_muted),
                        )
                        .on_click(|_, _, cx| cx.open_url(REPOSITORY_URL)),
                ),
            )
    }

    /// Where the update stands, with what can be done about it.
    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let state = updates::state(cx);
        let status = Status::of(&state, cx);
        let progress = match &state {
            UpdateState::Downloading {
                received, total, ..
            } => Some(*received as f32 / (*total).max(1) as f32),
            _ => None,
        };
        let notes = match &state {
            UpdateState::Available(update) => updates::notes_excerpt(&update.notes, 4),
            _ => None,
        };
        div()
            .py_2()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_start()
                            .child(
                                div().w(px(DOT_INSET)).pt(px(5.)).flex_shrink_0().child(
                                    div()
                                        .size(px(8.))
                                        .rounded_full()
                                        .bg(status.tone.color(&colors)),
                                ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div().font_weight(FontWeight::SEMIBOLD).child(status.title),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(if status.tone == Tone::Error {
                                                colors.error
                                            } else {
                                                colors.text_muted
                                            })
                                            .child(status.detail),
                                    ),
                            ),
                    )
                    .child(self.render_actions(&state, cx)),
            )
            .when_some(progress, |this, progress| {
                this.child(progress_bar(progress, cx))
            })
            .when_some(notes, |this, notes| {
                this.child(
                    div()
                        .ml(px(DOT_INSET))
                        .pl_3()
                        .border_l_2()
                        .border_color(colors.border)
                        .flex()
                        .flex_col()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .children(
                            notes
                                .lines()
                                .map(|line| div().child(line.to_owned()))
                                .collect::<Vec<_>>(),
                        ),
                )
            })
    }

    fn render_actions(&self, state: &UpdateState, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = div().flex().flex_shrink_0().items_center().gap_4();
        let check = |label: &'static str, cx: &mut Context<Self>| {
            link_action("check-update", label, Some(ICON_REFRESH), cx)
                .on_click(|_, _, cx| updates::check(true, cx))
        };
        match state {
            UpdateState::Idle | UpdateState::UpToDate => {
                actions.child(check("Check for Updates", cx))
            }
            UpdateState::Checking => actions.child(
                link_action("check-update", "Checking…", Some(ICON_REFRESH), cx)
                    .opacity(0.5)
                    .cursor_default(),
            ),
            UpdateState::Unavailable(version) => {
                let url = format!("{REPOSITORY_URL}/releases/tag/v{version}");
                actions
                    .child(
                        link_action("release-page", "Release Page", Some(ICON_EXTERNAL), cx)
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(check("Check Again", cx))
            }
            UpdateState::Available(update) => {
                let url = update.release_url.clone();
                actions
                    .child(
                        link_action("release-notes", "Release Notes", None, cx)
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(
                        button("about-update", "Update…", ButtonKind::Primary, cx)
                            .on_click(|_, _, cx| updates::activate(cx)),
                    )
            }
            UpdateState::Downloading { .. } => actions.child(
                link_action("cancel-update", "Cancel", None, cx)
                    .on_click(|_, _, cx| updates::cancel(cx)),
            ),
            UpdateState::Ready(..) => actions.child(
                button("about-update", "Restart to Update", ButtonKind::Primary, cx)
                    .on_click(|_, _, cx| updates::activate(cx)),
            ),
            UpdateState::Installing => actions,
            UpdateState::Failed(_) => actions.child(check("Check for Updates", cx)).when(
                updates::retry_available(cx),
                |this| {
                    this.child(
                        button("about-update", "Retry", ButtonKind::Primary, cx)
                            .on_click(|_, _, cx| updates::activate(cx)),
                    )
                },
            ),
        }
    }

    fn render_channel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let selected = updates::channel(cx);
        let installing = updates::state(cx) == UpdateState::Installing;
        setting_row(
            "Update channel",
            "Preview adds release candidates. Switching never downgrades ZP.",
            cx,
        )
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .children(UpdateChannel::ALL.into_iter().map(|channel| {
                    let active = channel == selected;
                    div()
                        .id(("update-channel", channel as usize))
                        .px_3()
                        .py_1()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .when(active, |this| {
                            this.bg(colors.accent)
                                .text_color(colors.accent_text)
                                .font_weight(FontWeight::MEDIUM)
                        })
                        .when(!active, |this| {
                            this.hover_style(move |s| s.bg(colors.surface_hover))
                        })
                        .child(channel.label())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !installing && !active {
                                this.set_update_channel(channel, cx);
                            }
                        }))
                })),
        )
    }

    fn render_auto_check(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let auto = AppSettings::get(cx).check_for_updates.unwrap_or(true);
        setting_row(
            "Check automatically",
            "Looks for updates after startup and every six hours.",
            cx,
        )
        .child(
            div()
                .id("auto-update-checks")
                .cursor_pointer()
                .child(switch(auto, cx))
                .on_click(cx.listener(move |this, _, _, cx| this.set_auto_updates(!auto, cx))),
        )
    }
}

/// The color of the status dot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Neutral,
    Accent,
    Success,
    Warning,
    Error,
}

impl Tone {
    fn color(self, colors: &crate::theme::ThemeColors) -> Rgba {
        match self {
            Self::Neutral => colors.text_muted,
            Self::Accent => colors.accent,
            Self::Success => colors.success,
            Self::Warning => colors.warning,
            Self::Error => colors.error,
        }
    }
}

/// What the page says about an [`UpdateState`].
struct Status {
    tone: Tone,
    title: String,
    detail: String,
}

impl Status {
    fn of(state: &UpdateState, cx: &App) -> Self {
        let checked = cx
            .global::<AppUpdate>()
            .last_checked
            .map(|time| format!("Last checked {}", ago(time.timestamp_millis())));
        let channel = format!("{} channel", updates::channel(cx).label());
        let (tone, title, detail) = match state {
            UpdateState::Idle => (
                Tone::Neutral,
                "Not checked yet".to_owned(),
                checked.unwrap_or(channel),
            ),
            UpdateState::Checking => (Tone::Accent, "Checking for updates…".to_owned(), channel),
            UpdateState::UpToDate => (
                Tone::Success,
                "ZP is up to date".to_owned(),
                checked.unwrap_or(channel),
            ),
            UpdateState::Unavailable(version) => (
                Tone::Warning,
                format!("ZP {version} is available"),
                "There is no package for this installation; download it from the release page."
                    .to_owned(),
            ),
            UpdateState::Available(update) => (
                Tone::Accent,
                format!("ZP {} is available", update.version),
                format!(
                    "{} · {}",
                    update.channel.label(),
                    megabytes(update.asset.size)
                ),
            ),
            UpdateState::Downloading {
                update,
                received,
                total,
            } => (
                Tone::Accent,
                format!("Downloading ZP {}", update.version),
                format!(
                    "{} of {} · {}%",
                    megabytes(*received),
                    megabytes(*total),
                    received.saturating_mul(100) / (*total).max(1)
                ),
            ),
            UpdateState::Ready(update, _) => (
                Tone::Success,
                format!("ZP {} is ready to install", update.version),
                "Restart ZP to finish updating; your workspace is restored.".to_owned(),
            ),
            UpdateState::Installing => (
                Tone::Accent,
                "Installing the update…".to_owned(),
                "ZP restarts once the new version is ready.".to_owned(),
            ),
            UpdateState::Failed(error) => (Tone::Error, "Update failed".to_owned(), error.clone()),
        };
        Self {
            tone,
            title,
            detail,
        }
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// The commit this build was made from, if known.
fn build_commit() -> Option<&'static str> {
    let commit = env!("ZP_BUILD_COMMIT");
    (!commit.is_empty() && commit != "unknown").then_some(commit)
}

/// What a bug report needs to know about this build.
fn version_info() -> String {
    format!(
        "{} {} ({})\nBuild: {}\nPlatform: {}\nPackage: {:?}",
        zp_core::APP_NAME,
        zp_core::VERSION,
        UpdateChannel::default().label(),
        build_commit().unwrap_or("unknown"),
        zp_core::update::platform_key(),
        zp_core::update::PackageKind::current(),
    )
}

/// A row of the update preferences: title and description, its control appended.
fn setting_row(title: &str, description: &str, cx: &App) -> gpui::Div {
    let colors = &cx.theme().colors;
    div().py_2().flex().items_center().gap_6().child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(title.to_owned())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(description.to_owned()),
            ),
    )
}

fn pill(text: String, background: Rgba, foreground: Rgba) -> impl IntoElement {
    div()
        .px_2()
        .py(px(1.))
        .rounded_full()
        .bg(background)
        .text_color(foreground)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .child(text)
}

/// An accent text action with an optional leading icon.
fn link_action(
    id: &'static str,
    label: &'static str,
    icon: Option<&'static [u8]>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_1p5()
        .px_2()
        .py_1()
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(colors.accent)
        .font_weight(FontWeight::MEDIUM)
        .hover_style(move |s| s.bg(colors.surface_hover))
        .when_some(icon, |this, icon| {
            this.child(svg().data(icon).size(px(13.)).text_color(colors.accent))
        })
        .child(label)
}

fn icon_button(id: &'static str, icon: &'static [u8], cx: &App) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    div()
        .id(id)
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .hover_style(move |s| s.bg(colors.surface_hover))
        .child(svg().data(icon).size(px(12.)).text_color(colors.text_muted))
}

/// An on/off switch (the element around it handles the click).
fn switch(on: bool, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    div()
        .w(px(32.))
        .h(px(18.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .p(px(2.))
        .rounded_full()
        .bg(if on { colors.accent } else { colors.border })
        .when(on, |this| this.justify_end())
        .child(div().size(px(14.)).rounded_full().bg(if on {
            colors.accent_text
        } else {
            colors.text_muted
        }))
}

fn progress_bar(fraction: f32, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    div()
        .ml(px(DOT_INSET))
        .h(px(6.))
        .rounded_full()
        .bg(colors.border)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(gpui::relative(fraction.clamp(0., 1.)))
                .rounded_full()
                .bg(colors.accent),
        )
}
