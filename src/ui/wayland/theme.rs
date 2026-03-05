use crate::{ThemeColorsConfig, ThemeMode};

/// Per-color theme values resolved from the configured mode and overrides.
#[derive(Debug, Clone, Copy)]
pub(super) struct ThemeColors {
    pub bg_base: iced::Color,
    pub bg_input: iced::Color,
    pub accent: iced::Color,
    pub accent_hover: iced::Color,
    pub text_main: iced::Color,
    pub text_muted: iced::Color,
    pub selection_bg: iced::Color,
    pub border: iced::Color,
}

impl ThemeColors {
    fn dark() -> Self {
        Self {
            bg_base: iced::Color {
                r: 0.15,
                g: 0.16,
                b: 0.21,
                a: 0.95,
            },
            bg_input: iced::Color {
                r: 0.26,
                g: 0.27,
                b: 0.35,
                a: 1.0,
            },
            accent: iced::Color {
                r: 0.74,
                g: 0.57,
                b: 0.97,
                a: 1.0,
            },
            accent_hover: iced::Color {
                r: 0.54,
                g: 0.91,
                b: 0.99,
                a: 1.0,
            },
            text_main: iced::Color::WHITE,
            text_muted: iced::Color {
                r: 0.38,
                g: 0.44,
                b: 0.64,
                a: 1.0,
            },
            selection_bg: iced::Color {
                r: 0.27,
                g: 0.29,
                b: 0.36,
                a: 0.8,
            },
            border: iced::Color {
                r: 0.38,
                g: 0.44,
                b: 0.64,
                a: 0.5,
            },
        }
    }

    fn light() -> Self {
        Self {
            bg_base: iced::Color::from_rgb(
                0xfa as f32 / 255.0,
                0xf4 as f32 / 255.0,
                0xed as f32 / 255.0,
            ),
            bg_input: iced::Color::from_rgb(
                0xff as f32 / 255.0,
                0xfa as f32 / 255.0,
                0xf3 as f32 / 255.0,
            ),
            accent: iced::Color::from_rgb(
                0x90 as f32 / 255.0,
                0x7a as f32 / 255.0,
                0xa9 as f32 / 255.0,
            ),
            accent_hover: iced::Color::from_rgb(
                0x56 as f32 / 255.0,
                0x94 as f32 / 255.0,
                0x9f as f32 / 255.0,
            ),
            text_main: iced::Color::from_rgb(
                0x57 as f32 / 255.0,
                0x52 as f32 / 255.0,
                0x79 as f32 / 255.0,
            ),
            text_muted: iced::Color::from_rgb(
                0x98 as f32 / 255.0,
                0x93 as f32 / 255.0,
                0xa5 as f32 / 255.0,
            ),
            selection_bg: iced::Color::from_rgb(
                0xdf as f32 / 255.0,
                0xda as f32 / 255.0,
                0xd9 as f32 / 255.0,
            ),
            border: iced::Color::from_rgb(
                0x79 as f32 / 255.0,
                0x75 as f32 / 255.0,
                0x93 as f32 / 255.0,
            ),
        }
    }

    fn from_mode(mode: &ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
        }
    }

    pub fn from_mode_with_overrides(
        mode: &ThemeMode,
        overrides: Option<&ThemeColorsConfig>,
    ) -> Self {
        let mut colors = Self::from_mode(mode);
        if let Some(ov) = overrides {
            if let Some(c) = ov.bg_base.as_deref().and_then(parse_hex_color) {
                colors.bg_base = c;
            }
            if let Some(c) = ov.bg_input.as_deref().and_then(parse_hex_color) {
                colors.bg_input = c;
            }
            if let Some(c) = ov.accent.as_deref().and_then(parse_hex_color) {
                colors.accent = c;
            }
            if let Some(c) = ov.accent_hover.as_deref().and_then(parse_hex_color) {
                colors.accent_hover = c;
            }
            if let Some(c) = ov.text_main.as_deref().and_then(parse_hex_color) {
                colors.text_main = c;
            }
            if let Some(c) = ov.text_muted.as_deref().and_then(parse_hex_color) {
                colors.text_muted = c;
            }
            if let Some(c) = ov.selection_bg.as_deref().and_then(parse_hex_color) {
                colors.selection_bg = c;
            }
            if let Some(c) = ov.border.as_deref().and_then(parse_hex_color) {
                colors.border = c;
            }
        }
        colors
    }
}

fn parse_hex_color(hex: &str) -> Option<iced::Color> {
    let hex = hex.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some(iced::Color::from_rgba8(r * 17, g * 17, b * 17, 1.0))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(iced::Color::from_rgba8(r, g, b, 1.0))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(iced::Color::from_rgba8(r, g, b, a as f32 / 255.0))
        }
        _ => None,
    }
}
