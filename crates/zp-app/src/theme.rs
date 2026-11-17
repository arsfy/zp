//! Theme tokens for the ZP UI.
//!
//! The active theme is stored as a GPUI global; it follows the system appearance unless
//! the settings pick light or dark.

use gpui::{App, Global, Pixels, Rgba, SharedString, WindowAppearance, px, rgb};
use zp_core::settings::ThemeChoice;

/// Color tokens used throughout the UI.
#[derive(Clone, Debug)]
pub struct ThemeColors {
    /// Window background.
    pub background: Rgba,
    /// Raised surfaces such as panels, cards and the tab bar.
    pub surface: Rgba,
    /// Project status bar background.
    pub status_bar: Rgba,
    /// Thin divider above the project status bar.
    pub status_bar_border: Rgba,
    /// Hovered surface.
    pub surface_hover: Rgba,
    /// Borders and dividers.
    pub border: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Secondary / muted text.
    pub text_muted: Rgba,
    /// Accent color for primary actions and focus.
    pub accent: Rgba,
    /// Text drawn on top of the accent color.
    pub accent_text: Rgba,
    /// Success state (e.g. added lines).
    pub success: Rgba,
    /// Warning state.
    pub warning: Rgba,
    /// Error state (e.g. removed lines).
    pub error: Rgba,
    /// Background of code blocks and inline code.
    pub code_background: Rgba,
}

/// Spacing and sizing tokens.
#[derive(Clone, Debug)]
pub struct ThemeMetrics {
    /// Base UI font size.
    pub font_size: Pixels,
    /// Corner radius for controls.
    pub radius: Pixels,
    /// Monospace font for code and command output.
    pub mono_font: SharedString,
}

/// The active UI theme.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Whether this is a dark theme.
    pub is_dark: bool,
    /// Color tokens.
    pub colors: ThemeColors,
    /// Spacing and sizing tokens.
    pub metrics: ThemeMetrics,
}

impl Global for Theme {}

impl Theme {
    /// Installs the theme of the settings (or the system appearance).
    pub fn init(cx: &mut App) {
        let theme = Self::for_settings(cx.window_appearance(), cx);
        cx.set_global(theme);
    }

    fn choice(cx: &App) -> ThemeChoice {
        cx.try_global::<crate::store::AppSettings>()
            .and_then(|_| crate::store::AppSettings::get(cx).theme)
            .unwrap_or_default()
    }

    fn for_settings(appearance: WindowAppearance, cx: &App) -> Self {
        match Self::choice(cx) {
            ThemeChoice::System => Self::for_appearance(appearance),
            ThemeChoice::Dark => Self::dark(),
            ThemeChoice::Light => Self::light(),
        }
    }

    /// Switches the global theme if the system appearance changed (and the settings
    /// follow it). Returns `true` when the theme was replaced and windows need to
    /// re-render.
    pub fn sync_with_appearance(appearance: WindowAppearance, cx: &mut App) -> bool {
        let next = Self::for_settings(appearance, cx);
        if cx.global::<Self>().is_dark == next.is_dark {
            return false;
        }
        cx.set_global(next);
        true
    }

    /// Applies a changed theme setting to every window.
    pub fn apply_settings(cx: &mut App) {
        if Self::sync_with_appearance(cx.window_appearance(), cx) {
            cx.refresh_windows();
        }
    }

    /// Returns the theme for a system appearance.
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    /// The default dark theme.
    pub fn dark() -> Self {
        Self {
            is_dark: true,
            colors: ThemeColors {
                background: rgb(0x16171b),
                surface: rgb(0x1e2026),
                status_bar: rgb(0x2f3135),
                status_bar_border: rgb(0x45474b),
                surface_hover: rgb(0x2a2d35),
                border: rgb(0x2e3139),
                text: rgb(0xe6e7eb),
                text_muted: rgb(0x8b8f9a),
                accent: rgb(0x6e8cff),
                accent_text: rgb(0xffffff),
                success: rgb(0x4fbf7f),
                warning: rgb(0xe0b350),
                error: rgb(0xe5605b),
                code_background: rgb(0x23252c),
            },
            metrics: ThemeMetrics::default(),
        }
    }

    /// The default light theme.
    pub fn light() -> Self {
        Self {
            is_dark: false,
            colors: ThemeColors {
                background: rgb(0xffffff),
                surface: rgb(0xf5f6f8),
                status_bar: rgb(0xe9eaec),
                status_bar_border: rgb(0xd1d3d7),
                surface_hover: rgb(0xeaecf0),
                border: rgb(0xdfe2e7),
                text: rgb(0x1d1f24),
                text_muted: rgb(0x6b7280),
                accent: rgb(0x3d63f5),
                accent_text: rgb(0xffffff),
                success: rgb(0x22863a),
                warning: rgb(0xb7791f),
                error: rgb(0xcb2431),
                code_background: rgb(0xf0f1f4),
            },
            metrics: ThemeMetrics::default(),
        }
    }
}

impl Default for ThemeMetrics {
    fn default() -> Self {
        Self {
            font_size: px(13.),
            radius: px(6.),
            mono_font: if cfg!(target_os = "macos") {
                "Menlo"
            } else if cfg!(target_os = "windows") {
                "Consolas"
            } else {
                "DejaVu Sans Mono"
            }
            .into(),
        }
    }
}

/// Convenience accessor: `cx.theme()`.
pub trait ActiveTheme {
    /// Returns the active theme.
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}
