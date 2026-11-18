//! Small shared UI pieces.

use gpui::{
    AnyView, App, ClickEvent, Context, Div, ElementId, Font, IntoElement, MouseButton, Pixels,
    Render, Rgba, SharedString, Stateful, Svg, Window, div, font, prelude::*, px, svg,
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

/// Hides the provider prefix when the model name already has a namespace.
pub fn model_display_name(qualified_id: &str) -> &str {
    if let Some((_, model)) = qualified_id.split_once('/')
        && model.matches('/').count() == 1
    {
        model
    } else {
        qualified_id
    }
}

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

/// The ZP logo (`assets/icon.png`), decoded once.
pub fn app_logo(cx: &App) -> Option<std::sync::Arc<gpui::RenderImage>> {
    static LOGO: std::sync::OnceLock<Option<std::sync::Arc<gpui::RenderImage>>> =
        std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            include_bytes!("../../../assets/icon.png").to_vec(),
        )
        .to_image_data(cx.svg_renderer())
        .inspect_err(|error| tracing::warn!(%error, "could not decode the ZP logo"))
        .ok()
    })
    .clone()
}

/// The Update button and the settings gear, at the end of a custom title row (macOS).
pub fn title_bar_controls(cx: &App) -> Div {
    window_controls(false, cx)
}

/// The same controls, compact, where the system draws the title bar (the status bar).
pub fn status_bar_controls(cx: &App) -> Div {
    window_controls(true, cx)
}

fn window_controls(compact: bool, cx: &App) -> Div {
    let colors = cx.theme().colors.clone();
    div()
        .flex()
        .items_center()
        .gap(px(if compact { 4. } else { 8. }))
        .flex_shrink_0()
        .children(crate::updates::title_bar_button(compact, cx))
        .child(
            div()
                .id("open-settings")
                .size(px(if compact { 20. } else { 28. }))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(tooltip("Settings"))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| {
                    cx.stop_propagation();
                    crate::settings::SettingsWindow::open(cx);
                })
                .child(
                    svg()
                        .data(include_bytes!("../../../assets/icons/settings.svg"))
                        .text_color(colors.text_muted)
                        .size(px(if compact { 13. } else { 16. })),
                ),
        )
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

/// A checkbox whose selected state is supplied by the caller.
pub fn checkbox(id: &'static str, label: &str, checked: bool, cx: &App) -> Stateful<Div> {
    use gpui::accesskit::{Role, Toggled};

    let colors = &cx.theme().colors;
    div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(label.to_owned())
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            div()
                .size(px(14.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(if checked {
                    colors.accent
                } else {
                    colors.text_muted
                })
                .when(checked, |this| {
                    this.bg(colors.accent)
                        .text_color(colors.accent_text)
                        .text_size(px(10.))
                        .child("✓")
                }),
        )
        .child(label.to_owned())
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

const ICON_DISCLOSURE_OPEN: &[u8] = include_bytes!("../../../assets/icons/chevron-down.svg");
const ICON_DISCLOSURE_CLOSED: &[u8] = include_bytes!("../../../assets/icons/chevron-right.svg");

/// The font of ordinary UI text (the platform's system font).
pub fn ui_font() -> Font {
    font(".SystemUIFont")
}

/// How far below the middle of its line box the baseline of `font` at `size` sits.
///
/// gpui centers a font's ascent + descent in the line box, so this is
/// `(ascent - descent) / 2`. It differs between fonts: mixing e.g. Segoe UI with Consolas in
/// an `items_center` row puts their baselines a few pixels apart.
fn baseline_below_middle(cx: &App, font: &Font, size: Pixels) -> Pixels {
    let text_system = cx.text_system();
    let id = text_system.resolve_font(font);
    (text_system.ascent(id, size) - text_system.descent(id, size)) / 2.
}

/// Vertical offset (for `.relative().top(…)`) that puts the baseline of single-line text
/// in `text` font and size on the baseline of `reference` text, when both are items of the
/// same `items_center` flex row.
pub fn baseline_shift(cx: &App, reference: (&Font, Pixels), text: (&Font, Pixels)) -> Pixels {
    baseline_below_middle(cx, reference.0, reference.1) - baseline_below_middle(cx, text.0, text.1)
}

/// Vertical offset (for `.relative().top(…)`) that centers an icon on the x-height of
/// `text` when both are items of the same `items_center` flex row.
pub fn icon_shift(cx: &App, text: (&Font, Pixels)) -> Pixels {
    let text_system = cx.text_system();
    let id = text_system.resolve_font(text.0);
    baseline_below_middle(cx, text.0, text.1) - text_system.x_height(id, text.1) / 2.
}

/// A disclosure chevron (right when collapsed, down when expanded), lined up with `text`
/// in an `items_center` row. An icon instead of ▸/▾: those glyphs come from a fallback
/// font whose metrics differ per platform, which made them sit visibly off-center.
pub fn disclosure_icon(expanded: bool, color: Rgba, text: (&Font, Pixels), cx: &App) -> Svg {
    svg()
        .data(if expanded {
            ICON_DISCLOSURE_OPEN
        } else {
            ICON_DISCLOSURE_CLOSED
        })
        .size(px(12.))
        .flex_shrink_0()
        .relative()
        .top(icon_shift(cx, text))
        .text_color(color)
}

/// `1234` → `1.2k`, `2500000` → `2.5M`.
pub fn format_tokens(tokens: u64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1_000.0),
        t => t.to_string(),
    }
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
