//! Theme tokens for the ZP UI.
//!
//! The active theme is stored as a GPUI global and follows the system appearance.

use gpui::{App, Global, Pixels, Rgba, WindowAppearance, px, rgb};

/// Color tokens used throughout the UI.
#[derive(Clone, Debug)]
#[allow(dead_code, reason = "state colors are used from M5 on")]
pub struct ThemeColors {
    /// Window background.
    pub background: Rgba,
    /// Raised surfaces such as panels, cards and the tab bar.
    pub surface: Rgba,
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
}

/// Spacing and sizing tokens.
#[derive(Clone, Debug)]
pub struct ThemeMetrics {
    /// Base UI font size.
    pub font_size: Pixels,
    /// Corner radius for controls.
    pub radius: Pixels,
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
    /// Installs the theme matching the current system appearance.
    pub fn init(cx: &mut App) {
        cx.set_global(Self::for_appearance(cx.window_appearance()));
    }

    /// Switches the global theme if the system appearance changed. Returns `true` when
    /// the theme was replaced and windows need to re-render.
    pub fn sync_with_appearance(appearance: WindowAppearance, cx: &mut App) -> bool {
        let next = Self::for_appearance(appearance);
        if cx.global::<Self>().is_dark == next.is_dark {
            return false;
        }
        cx.set_global(next);
        true
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
                surface_hover: rgb(0x2a2d35),
                border: rgb(0x2e3139),
                text: rgb(0xe6e7eb),
                text_muted: rgb(0x8b8f9a),
                accent: rgb(0x6e8cff),
                accent_text: rgb(0xffffff),
                success: rgb(0x4fbf7f),
                warning: rgb(0xe0b350),
                error: rgb(0xe5605b),
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
                surface_hover: rgb(0xeaecf0),
                border: rgb(0xdfe2e7),
                text: rgb(0x1d1f24),
                text_muted: rgb(0x6b7280),
                accent: rgb(0x3d63f5),
                accent_text: rgb(0xffffff),
                success: rgb(0x22863a),
                warning: rgb(0xb7791f),
                error: rgb(0xcb2431),
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
