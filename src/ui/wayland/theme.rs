use crate::{PreviewConfig, ThemeColorsConfig, ThemeMode};

/// Resolved appearance of the right-hand preview pane.
#[derive(Debug, Clone)]
pub(super) struct PreviewStyle {
    /// Base text size (body text, metadata values).
    pub text: f32,
    /// Title size.
    pub title: f32,
    /// Subtitle, headings, metadata keys.
    pub small: f32,
    /// Hero glyph size (emoji).
    pub hero_glyph: f32,
    /// Hero value size (calculator/currency results).
    pub hero_value: f32,
    /// Header icon size.
    pub icon: f32,
    /// Width of the metadata key column.
    pub key_width: f32,
    /// Pane width as a portion of the window, 0.1..=0.8.
    pub width_fraction: f32,
    pub font: iced::Font,
    pub mono_font: iced::Font,
    pub bg: iced::Color,
    pub bg_block: iced::Color,
    pub text_main: iced::Color,
    pub text_muted: iced::Color,
    pub accent: iced::Color,
    pub border: iced::Color,
}

impl PreviewStyle {
    pub const DEFAULT_FONT_SIZE: f32 = 16.0;
    pub const DEFAULT_WIDTH_PERCENT: f32 = 50.0;

    /// Build the preview style from the UI theme and the optional `general.preview` config.
    pub fn resolve(
        theme: &ThemeColors,
        config: &PreviewConfig,
        ui_font_family: Option<&str>,
    ) -> Self {
        let base = config
            .font_size
            .filter(|size| size.is_finite() && *size > 0.0)
            .unwrap_or(Self::DEFAULT_FONT_SIZE);
        let width_percent = config
            .width_percent
            .filter(|p| p.is_finite())
            .unwrap_or(Self::DEFAULT_WIDTH_PERCENT)
            .clamp(10.0, 80.0);

        let colors = config.colors.as_ref();
        let color = |value: Option<&String>, fallback: iced::Color| {
            value
                .map(String::as_str)
                .and_then(parse_hex_color)
                .unwrap_or(fallback)
        };

        let font = config
            .font_family
            .as_deref()
            .or(ui_font_family)
            .map(leaked_font)
            .unwrap_or_default();
        let mono_font = config
            .mono_font_family
            .as_deref()
            .map(leaked_font)
            .unwrap_or(iced::Font::MONOSPACE);

        Self {
            text: base,
            title: (base * 1.4).round(),
            small: (base * 0.8).round(),
            hero_glyph: (base * 5.0).round(),
            hero_value: (base * 2.2).round(),
            icon: (base * 3.0).round(),
            key_width: (base * 7.0).round(),
            width_fraction: width_percent / 100.0,
            font,
            mono_font,
            bg: color(
                colors.and_then(|c| c.bg.as_ref()),
                iced::Color {
                    a: 0.25,
                    ..theme.bg_input
                },
            ),
            bg_block: color(
                colors.and_then(|c| c.bg_block.as_ref()),
                iced::Color {
                    a: 0.75,
                    ..theme.bg_input
                },
            ),
            text_main: color(colors.and_then(|c| c.text_main.as_ref()), theme.text_main),
            text_muted: color(colors.and_then(|c| c.text_muted.as_ref()), theme.text_muted),
            accent: color(colors.and_then(|c| c.accent.as_ref()), theme.accent),
            border: color(colors.and_then(|c| c.border.as_ref()), theme.border),
        }
    }
}

/// iced fonts need a `'static` family name; the preview style is built once per run.
fn leaked_font(family: &str) -> iced::Font {
    let family: &'static str = Box::leak(family.to_string().into_boxed_str());
    iced::Font {
        family: iced::font::Family::Name(family),
        ..iced::Font::default()
    }
}

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
    /// Default dark theme: Sleek Obsidian (deep neutral charcoal, high-contrast
    /// silver-slate secondary text, and refined electric indigo/violet accent).
    fn dark() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x13, 0x14, 0x19, 0.96),
            bg_input: iced::Color::from_rgba8(0x1C, 0x1E, 0x26, 1.0),
            accent: iced::Color::from_rgba8(0x8B, 0x7C, 0xFF, 1.0),
            accent_hover: iced::Color::from_rgba8(0xA5, 0x99, 0xFF, 1.0),
            text_main: iced::Color::from_rgba8(0xF2, 0xF4, 0xF8, 1.0),
            text_muted: iced::Color::from_rgba8(0x94, 0x9C, 0xB0, 1.0),
            selection_bg: iced::Color::from_rgba8(0x25, 0x28, 0x36, 0.92),
            border: iced::Color::from_rgba8(0x2D, 0x32, 0x42, 0.75),
        }
    }

    /// Default light theme: Rosé Pine Dawn with improved secondary text legibility.
    fn light() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0xFA, 0xF4, 0xED, 0.98),
            bg_input: iced::Color::from_rgba8(0xFF, 0xFA, 0xF3, 1.0),
            accent: iced::Color::from_rgba8(0x79, 0x5E, 0x9E, 1.0),
            accent_hover: iced::Color::from_rgba8(0x28, 0x69, 0x83, 1.0),
            text_main: iced::Color::from_rgba8(0x46, 0x42, 0x61, 1.0),
            text_muted: iced::Color::from_rgba8(0x79, 0x75, 0x93, 1.0),
            selection_bg: iced::Color::from_rgba8(0xEB, 0xE5, 0xDF, 1.0),
            border: iced::Color::from_rgba8(0xCE, 0xCA, 0xCD, 0.85),
        }
    }

    fn catppuccin_mocha() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x1E, 0x1E, 0x2E, 0.96),
            bg_input: iced::Color::from_rgba8(0x31, 0x32, 0x44, 1.0),
            accent: iced::Color::from_rgba8(0xCB, 0xA6, 0xF7, 1.0),
            accent_hover: iced::Color::from_rgba8(0x89, 0xB4, 0xFA, 1.0),
            text_main: iced::Color::from_rgba8(0xCD, 0xD6, 0xF4, 1.0),
            text_muted: iced::Color::from_rgba8(0xA6, 0xAD, 0xC8, 1.0),
            selection_bg: iced::Color::from_rgba8(0x31, 0x32, 0x44, 0.90),
            border: iced::Color::from_rgba8(0x45, 0x47, 0x5A, 0.80),
        }
    }

    fn catppuccin_latte() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0xEF, 0xF1, 0xF5, 0.98),
            bg_input: iced::Color::from_rgba8(0xE6, 0xE9, 0xEF, 1.0),
            accent: iced::Color::from_rgba8(0x88, 0x39, 0xEF, 1.0),
            accent_hover: iced::Color::from_rgba8(0x1E, 0x66, 0xF5, 1.0),
            text_main: iced::Color::from_rgba8(0x4C, 0x4F, 0x69, 1.0),
            text_muted: iced::Color::from_rgba8(0x6C, 0x6F, 0x85, 1.0),
            selection_bg: iced::Color::from_rgba8(0xDC, 0xE0, 0xE8, 1.0),
            border: iced::Color::from_rgba8(0xBC, 0xC0, 0xCC, 0.85),
        }
    }

    fn tokyo_night() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x1A, 0x1B, 0x26, 0.96),
            bg_input: iced::Color::from_rgba8(0x24, 0x28, 0x3B, 1.0),
            accent: iced::Color::from_rgba8(0x7A, 0xA2, 0xF7, 1.0),
            accent_hover: iced::Color::from_rgba8(0xBB, 0x9A, 0xF7, 1.0),
            text_main: iced::Color::from_rgba8(0xC0, 0xCA, 0xF5, 1.0),
            text_muted: iced::Color::from_rgba8(0x82, 0x89, 0xB0, 1.0),
            selection_bg: iced::Color::from_rgba8(0x29, 0x2E, 0x42, 0.92),
            border: iced::Color::from_rgba8(0x3B, 0x42, 0x61, 0.80),
        }
    }

    fn nord() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x2E, 0x34, 0x40, 0.96),
            bg_input: iced::Color::from_rgba8(0x3B, 0x42, 0x52, 1.0),
            accent: iced::Color::from_rgba8(0x88, 0xC0, 0xD0, 1.0),
            accent_hover: iced::Color::from_rgba8(0x81, 0xA1, 0xC1, 1.0),
            text_main: iced::Color::from_rgba8(0xEC, 0xEF, 0xF4, 1.0),
            text_muted: iced::Color::from_rgba8(0x9C, 0xA6, 0xBA, 1.0),
            selection_bg: iced::Color::from_rgba8(0x43, 0x4C, 0x5E, 0.92),
            border: iced::Color::from_rgba8(0x4C, 0x56, 0x6A, 0.85),
        }
    }

    fn gruvbox() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x28, 0x28, 0x28, 0.96),
            bg_input: iced::Color::from_rgba8(0x3C, 0x38, 0x36, 1.0),
            accent: iced::Color::from_rgba8(0xFA, 0xBD, 0x2F, 1.0),
            accent_hover: iced::Color::from_rgba8(0xFE, 0x80, 0x19, 1.0),
            text_main: iced::Color::from_rgba8(0xEB, 0xDB, 0xB2, 1.0),
            text_muted: iced::Color::from_rgba8(0xA8, 0x99, 0x84, 1.0),
            selection_bg: iced::Color::from_rgba8(0x50, 0x49, 0x45, 0.90),
            border: iced::Color::from_rgba8(0x66, 0x5C, 0x54, 0.75),
        }
    }

    fn dracula() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x28, 0x2A, 0x36, 0.96),
            bg_input: iced::Color::from_rgba8(0x34, 0x37, 0x46, 1.0),
            accent: iced::Color::from_rgba8(0xBD, 0x93, 0xF9, 1.0),
            accent_hover: iced::Color::from_rgba8(0x8B, 0xE9, 0xFD, 1.0),
            text_main: iced::Color::from_rgba8(0xF8, 0xF8, 0xF2, 1.0),
            text_muted: iced::Color::from_rgba8(0x95, 0xA1, 0xC9, 1.0),
            selection_bg: iced::Color::from_rgba8(0x44, 0x47, 0x5A, 0.85),
            border: iced::Color::from_rgba8(0x62, 0x72, 0xA4, 0.55),
        }
    }

    fn rose_pine() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x19, 0x17, 0x24, 0.96),
            bg_input: iced::Color::from_rgba8(0x1F, 0x1D, 0x2E, 1.0),
            accent: iced::Color::from_rgba8(0xC4, 0xA7, 0xE7, 1.0),
            accent_hover: iced::Color::from_rgba8(0xEB, 0xBC, 0xBA, 1.0),
            text_main: iced::Color::from_rgba8(0xE0, 0xDE, 0xF4, 1.0),
            text_muted: iced::Color::from_rgba8(0x90, 0x8C, 0xAA, 1.0),
            selection_bg: iced::Color::from_rgba8(0x26, 0x23, 0x3A, 0.92),
            border: iced::Color::from_rgba8(0x40, 0x3D, 0x52, 0.80),
        }
    }

    fn kanagawa() -> Self {
        Self {
            bg_base: iced::Color::from_rgba8(0x1F, 0x1F, 0x28, 0.96),
            bg_input: iced::Color::from_rgba8(0x2A, 0x2A, 0x37, 1.0),
            accent: iced::Color::from_rgba8(0x95, 0x7F, 0xB8, 1.0),
            accent_hover: iced::Color::from_rgba8(0x7E, 0x9C, 0xD8, 1.0),
            text_main: iced::Color::from_rgba8(0xDC, 0xD7, 0xBA, 1.0),
            text_muted: iced::Color::from_rgba8(0x93, 0x8A, 0xA9, 1.0),
            selection_bg: iced::Color::from_rgba8(0x2D, 0x4F, 0x67, 0.70),
            border: iced::Color::from_rgba8(0x54, 0x54, 0x6D, 0.75),
        }
    }

    fn from_mode(mode: &ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
            ThemeMode::CatppuccinMocha => Self::catppuccin_mocha(),
            ThemeMode::CatppuccinLatte => Self::catppuccin_latte(),
            ThemeMode::TokyoNight => Self::tokyo_night(),
            ThemeMode::Nord => Self::nord(),
            ThemeMode::Gruvbox => Self::gruvbox(),
            ThemeMode::Dracula => Self::dracula(),
            ThemeMode::RosePine => Self::rose_pine(),
            ThemeMode::Kanagawa => Self::kanagawa(),
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
