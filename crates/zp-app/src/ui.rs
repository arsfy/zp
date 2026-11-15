//! Small shared UI pieces.

use gpui::{
    AnyView, App, ClickEvent, Context, Div, ElementId, IntoElement, MouseButton, Render,
    SharedString, Stateful, Window, div, prelude::*, px,
};

use crate::theme::ActiveTheme;

/// Whether windows draw their own title bar. On macOS the system titlebar is transparent
/// and the traffic lights sit on our title row; elsewhere the native titlebar is kept.
pub const CUSTOM_TITLE_BAR: bool = cfg!(target_os = "macos");
/// Height of the custom title row.
pub const TITLE_BAR_HEIGHT: f32 = 36.0;
/// Where the macOS traffic lights are drawn (vertically centered in the title row).
pub const TRAFFIC_LIGHTS: (f32, f32) = (12.0, 11.0);
/// Space left of the title for the traffic lights.
const TRAFFIC_LIGHTS_WIDTH: f32 = 78.0;

/// A custom title row: drags the window, double-click zooms (per system preference).
pub fn title_bar(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(TITLE_BAR_HEIGHT))
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .pl(px(TRAFFIC_LIGHTS_WIDTH))
        .pr_3()
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            if event.click_count == 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}

/// A key binding (`secondary-shift-n`) as shown to users: `⇧⌘N` on macOS,
/// `Ctrl+Shift+N` elsewhere.
pub fn shortcut_label(binding: &str) -> String {
    let parts: Vec<&str> = binding.split('-').collect();
    let (key, modifiers) = parts.split_last().map_or(("", &[][..]), |(k, m)| (*k, m));
    let has = |name: &str| modifiers.iter().any(|m| m.eq_ignore_ascii_case(name));
    let key = match key {
        "enter" => "Enter".to_owned(),
        "escape" => "Esc".to_owned(),
        "tab" => "Tab".to_owned(),
        "space" => "Space".to_owned(),
        key => key.to_uppercase(),
    };
    if cfg!(target_os = "macos") {
        let mut label = String::new();
        if has("ctrl") {
            label.push('⌃');
        }
        if has("alt") {
            label.push('⌥');
        }
        if has("shift") {
            label.push('⇧');
        }
        if has("secondary") || has("cmd") {
            label.push('⌘');
        }
        label + &key
    } else {
        let mut parts = Vec::new();
        if has("secondary") || has("ctrl") {
            parts.push("Ctrl");
        }
        if has("alt") {
            parts.push("Alt");
        }
        if has("shift") {
            parts.push("Shift");
        }
        parts.push(&key);
        parts.join("+")
    }
}

/// "just now", "5m ago", "3h ago", "2d ago", or a date for older times.
pub fn relative_time(ms: i64, now_ms: i64) -> String {
    let seconds = (now_ms - ms).max(0) / 1000;
    match seconds {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 7 * 86_400 => format!("{}d ago", s / 86_400),
        _ => zp_session::time::format_iso(ms)[..10].to_owned(),
    }
}

/// A flat button with a label and an optional shortcut hint.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    shortcut: Option<&str>,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_3()
        .py_1p5()
        .rounded(cx.theme().metrics.radius)
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface)
        .cursor_pointer()
        .hover_style(|style| style.bg(colors.surface_hover))
        .child(div().child(label.into()))
        .when_some(shortcut, |this, shortcut| {
            this.child(
                div()
                    .text_color(colors.text_muted)
                    .child(shortcut_label(shortcut)),
            )
        })
        .on_click(on_click)
}

/// An uppercase section caption.
pub fn section_header(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_size(px(11.))
        .text_color(cx.theme().colors.text_muted)
        .child(label.into())
}

/// A plain text tooltip.
pub struct TextTooltip(SharedString);

impl Render for TextTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .px_2()
            .py_1()
            .max_w(px(320.))
            .rounded(px(4.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface)
            .shadow_md()
            .text_size(px(12.))
            .text_color(colors.text)
            .child(self.0.clone())
    }
}

/// Builds a [`TextTooltip`], for `.tooltip(…)`.
pub fn tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| TextTooltip(text.clone())).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_labels() {
        if cfg!(target_os = "macos") {
            assert_eq!(shortcut_label("secondary-shift-n"), "⇧⌘N");
            assert_eq!(shortcut_label("secondary-alt-b"), "⌥⌘B");
        } else {
            assert_eq!(shortcut_label("secondary-shift-n"), "Ctrl+Shift+N");
            assert_eq!(shortcut_label("secondary-alt-b"), "Ctrl+Alt+B");
        }
    }

    #[test]
    fn relative_times() {
        let now = 1_000_000_000_000;
        assert_eq!(relative_time(now - 5_000, now), "just now");
        assert_eq!(relative_time(now - 5 * 60_000, now), "5m ago");
        assert_eq!(relative_time(now - 3 * 3_600_000, now), "3h ago");
        assert_eq!(relative_time(now - 2 * 86_400_000, now), "2d ago");
        assert_eq!(relative_time(now - 30 * 86_400_000, now), "2001-08-10");
    }
}
