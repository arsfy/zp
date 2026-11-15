//! Placeholder welcome screen. Replaced by the real Welcome window in milestone M4.

use gpui::{Context, FontWeight, IntoElement, Render, Subscription, Window, div, prelude::*, px};

use crate::theme::{ActiveTheme, Theme};

/// Root view of the welcome window.
pub struct WelcomeView {
    _appearance_subscription: Subscription,
}

impl WelcomeView {
    /// Creates the view and keeps the theme in sync with the system appearance.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_window_appearance(window, |_, window, cx| {
            if Theme::sync_with_appearance(window.appearance(), cx) {
                cx.refresh_windows();
            }
        });
        Self {
            _appearance_subscription: subscription,
        }
    }
}

impl Render for WelcomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = &theme.colors;

        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(colors.background)
            .text_color(colors.text)
            .text_size(theme.metrics.font_size)
            .child(
                div()
                    .text_size(px(40.))
                    .font_weight(FontWeight::BOLD)
                    .child(zp_core::APP_NAME),
            )
            .child(
                div()
                    .text_color(colors.text_muted)
                    .child("A native GUI agent harness"),
            )
            .child(
                div()
                    .mt_4()
                    .text_color(colors.text_muted)
                    .child(format!("Version {}", zp_core::VERSION)),
            )
    }
}
