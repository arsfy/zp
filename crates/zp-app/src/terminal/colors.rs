//! The terminal's color palette: the 16 ANSI colors (matched to the theme's brightness),
//! the 256-color cube and grays, and the theme's foreground and background.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, Rgba, rgb_to_hsla};

/// VS Code's dark terminal colors.
const DARK: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666,
    0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xe5e5e5,
];
/// VS Code's light terminal colors.
const LIGHT: [u32; 16] = [
    0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, 0x666666,
    0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

fn rgb(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

fn to_rgb(color: Rgba) -> Rgb {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb {
        r: channel(color.red),
        g: channel(color.green),
        b: channel(color.blue),
    }
}

pub fn to_hsla(color: Rgb) -> Hsla {
    rgb_to_hsla(gpui::rgb(
        u32::from(color.r) << 16 | u32::from(color.g) << 8 | u32::from(color.b),
    ))
}

/// Indexed color `index` of the 256-color palette.
pub fn indexed(index: u8, ansi: &[u32; 16]) -> Rgb {
    match index {
        0..=15 => rgb(ansi[usize::from(index)]),
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            Rgb {
                r: level(i / 36),
                g: level(i / 6 % 6),
                b: level(i % 6),
            }
        }
        _ => {
            let gray = (index - 232) * 10 + 8;
            Rgb {
                r: gray,
                g: gray,
                b: gray,
            }
        }
    }
}

fn dim(color: Rgb) -> Rgb {
    let scale = |v: u8| (f32::from(v) * 0.66) as u8;
    Rgb {
        r: scale(color.r),
        g: scale(color.g),
        b: scale(color.b),
    }
}

/// Resolves terminal colors for one frame.
pub struct Palette<'a> {
    ansi: &'static [u32; 16],
    foreground: Rgb,
    background: Rgb,
    cursor: Rgb,
    /// Colors the program changed (OSC 4/10/11).
    overrides: &'a Colors,
}

impl<'a> Palette<'a> {
    pub fn new(theme: &crate::theme::Theme, overrides: &'a Colors) -> Self {
        Self {
            ansi: if theme.is_dark { &DARK } else { &LIGHT },
            foreground: to_rgb(theme.colors.text),
            background: to_rgb(theme.colors.background),
            cursor: to_rgb(theme.colors.text),
            overrides,
        }
    }

    pub fn background(&self) -> Rgb {
        self.overrides[NamedColor::Background].unwrap_or(self.background)
    }

    pub fn resolve(&self, color: Color) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(index) => {
                self.overrides[usize::from(index)].unwrap_or_else(|| indexed(index, self.ansi))
            }
            Color::Named(named) => self.named(named),
        }
    }

    pub fn named(&self, named: NamedColor) -> Rgb {
        if let Some(rgb) = self.overrides[named] {
            return rgb;
        }
        let index = named as usize;
        match named {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => dim(self.foreground),
            _ if index < 16 => rgb(self.ansi[index]),
            // DimBlack … DimWhite follow Cursor.
            _ => dim(rgb(self.ansi[index - NamedColor::DimBlack as usize])),
        }
    }

    /// The bright version of a normal ANSI color (bold text), if it is one.
    pub fn brighten(&self, color: Color) -> Color {
        match color {
            Color::Named(named) if (named as usize) < 8 => Color::Indexed(named as u8 + 8),
            Color::Named(NamedColor::Foreground) => Color::Named(NamedColor::BrightForeground),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_grays() {
        assert_eq!(indexed(16, &DARK), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            indexed(231, &DARK),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(indexed(196, &DARK), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(indexed(232, &DARK), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(indexed(1, &DARK), rgb(0xcd3131));
    }
}
