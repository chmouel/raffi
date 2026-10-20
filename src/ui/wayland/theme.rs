use super::ansi::AnsiPalette;
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
    /// Theme palette for basic ANSI colours, with entries too close to the
    /// code block background swapped for a readable one.
    pub ansi: AnsiPalette,
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

        let bg_block = color(
            colors.and_then(|c| c.bg_block.as_ref()),
            iced::Color {
                a: 0.75,
                ..theme.bg_input
            },
        );
        let text_muted = color(colors.and_then(|c| c.text_muted.as_ref()), theme.text_muted);
        let backdrop = blend(bg_block, theme.bg_base);
        let readable = |c: iced::Color| contrast(c, backdrop) >= MIN_ANSI_CONTRAST;
        let mut ansi = theme.ansi;
        for i in 0..16 {
            if readable(ansi[i]) {
                continue;
            }
            // A pale bright colour falls back to its normal shade, which
            // keeps the hue; only then to the muted text colour.
            ansi[i] = if i >= 8 && readable(ansi[i - 8]) {
                ansi[i - 8]
            } else {
                text_muted
            };
        }

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
            bg_block,
            text_main: color(colors.and_then(|c| c.text_main.as_ref()), theme.text_main),
            text_muted,
            accent: color(colors.and_then(|c| c.accent.as_ref()), theme.accent),
            border: color(colors.and_then(|c| c.border.as_ref()), theme.border),
            ansi,
        }
    }
}

/// Below this WCAG contrast ratio a palette colour is unreadable on the
/// block background: black on dark themes, white on light ones.
const MIN_ANSI_CONTRAST: f32 = 2.0;

/// `top` drawn over the opaque `bottom`.
fn blend(top: iced::Color, bottom: iced::Color) -> iced::Color {
    let mix = |t: f32, b: f32| t * top.a + b * (1.0 - top.a);
    iced::Color::from_rgb(
        mix(top.r, bottom.r),
        mix(top.g, bottom.g),
        mix(top.b, bottom.b),
    )
}

/// WCAG contrast ratio between two opaque colours, from 1 to 21.
fn contrast(a: iced::Color, b: iced::Color) -> f32 {
    let luminance = |c: iced::Color| {
        let channel = |v: f32| {
            if v <= 0.039_28 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    };
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
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
    /// Preset name, as written in the config (`catppuccin-mocha`, ...).
    pub name: &'static str,
    pub light: bool,
    /// The preset's 16-colour terminal palette (normal then bright),
    /// used for basic ANSI colours in previews and Script Filter rows.
    pub ansi: AnsiPalette,
}

/// Build a 16-colour palette from `0xRRGGBB` values.
fn palette(colors: [u32; 16]) -> AnsiPalette {
    colors.map(|c| iced::Color::from_rgb8((c >> 16) as u8, (c >> 8) as u8, c as u8))
}

impl ThemeColors {
    /// `RAFFI_THEME*` variables for preview commands, so they can match
    /// their own colours (bat theme, light or dark style) to raffi's.
    pub fn command_env(&self) -> [(String, String); 6] {
        let hex = |c: iced::Color| {
            let [r, g, b, _] = c.into_rgba8();
            format!("#{r:02x}{g:02x}{b:02x}")
        };
        let mode = if self.light { "light" } else { "dark" };
        [
            ("RAFFI_THEME".into(), self.name.into()),
            ("RAFFI_THEME_MODE".into(), mode.into()),
            ("RAFFI_THEME_FG".into(), hex(self.text_main)),
            ("RAFFI_THEME_MUTED".into(), hex(self.text_muted)),
            ("RAFFI_THEME_ACCENT".into(), hex(self.accent)),
            ("RAFFI_THEME_BG".into(), hex(self.bg_base)),
        ]
    }

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
            name: "dark",
            light: false,
            ansi: palette([
                0x2D3242, 0xFF6B7A, 0x7FD99A, 0xF5C66B, 0x7AA2FF, 0xB79CFF, 0x6FD3E0, 0xC8CDD8,
                0x949CB0, 0xFF8A96, 0x9BE5B0, 0xFFD88A, 0x9DBAFF, 0xC9B5FF, 0x93E1EC, 0xF2F4F8,
            ]),
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
            name: "light",
            light: true,
            ansi: palette([
                0xF2E9E1, 0xB4637A, 0x286983, 0xEA9D34, 0x56949F, 0x907AA9, 0xD7827E, 0x575279,
                0x9893A5, 0xB4637A, 0x286983, 0xEA9D34, 0x56949F, 0x907AA9, 0xD7827E, 0x575279,
            ]),
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
            name: "catppuccin-mocha",
            light: false,
            ansi: palette([
                0x45475A, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xBAC2DE,
                0x585B70, 0xF7AEC2, 0xC2ECBF, 0xFCD682, 0xAECCFC, 0xF398DA, 0xB1EAE1, 0xA6ADC8,
            ]),
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
            name: "catppuccin-latte",
            light: true,
            ansi: palette([
                0xBCC0CC, 0xD20F39, 0x40A02B, 0xDF8E1D, 0x1E66F5, 0xEA76CB, 0x179299, 0x5C5F77,
                0xACB0BE, 0xE7103F, 0x46B02F, 0xE49931, 0x3878F6, 0xEF95D7, 0x19A1A8, 0x6C6F85,
            ]),
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
            name: "tokyo-night",
            light: false,
            ansi: palette([
                0x15161E, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xA9B1D6,
                0x414868, 0xF7768E, 0x9ECE6A, 0xE0AF68, 0x7AA2F7, 0xBB9AF7, 0x7DCFFF, 0xC0CAF5,
            ]),
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
            name: "nord",
            light: false,
            ansi: palette([
                0x3B4252, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x88C0D0, 0xE5E9F0,
                0x596377, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x8FBCBB, 0xECEFF4,
            ]),
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
            name: "gruvbox",
            light: false,
            ansi: palette([
                0x282828, 0xCC241D, 0x98971A, 0xD79921, 0x458588, 0xB16286, 0x689D6A, 0xA89984,
                0x928374, 0xFB4934, 0xB8BB26, 0xFABD2F, 0x83A598, 0xD3869B, 0x8EC07C, 0xEBDBB2,
            ]),
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
            name: "dracula",
            light: false,
            ansi: palette([
                0x21222C, 0xFF5555, 0x50FA7B, 0xF1FA8C, 0xBD93F9, 0xFF79C6, 0x8BE9FD, 0xF8F8F2,
                0x6272A4, 0xFF6E6E, 0x69FF94, 0xFFFFA5, 0xD6ACFF, 0xFF92DF, 0xA4FFFF, 0xFFFFFF,
            ]),
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
            name: "rose-pine",
            light: false,
            ansi: palette([
                0x26233A, 0xEB6F92, 0x31748F, 0xF6C177, 0x9CCFD8, 0xC4A7E7, 0xEBBCBA, 0xE0DEF4,
                0x6E6A86, 0xEB6F92, 0x31748F, 0xF6C177, 0x9CCFD8, 0xC4A7E7, 0xEBBCBA, 0xE0DEF4,
            ]),
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
            name: "kanagawa",
            light: false,
            ansi: palette([
                0x090618, 0xC34043, 0x76946A, 0xC0A36E, 0x7E9CD8, 0x957FB8, 0x6A9589, 0xC8C093,
                0x727169, 0xE82424, 0x98BB6C, 0xE6C384, 0x7FB4CA, 0x938AA9, 0x7AA89F, 0xDCD7BA,
            ]),
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

    /// Scale the alpha of the background colours, leaving text, accents and
    /// borders opaque. `1.0` keeps the theme as designed.
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        let opacity = if opacity.is_finite() {
            opacity.clamp(0.0, 1.0)
        } else {
            1.0
        };
        for c in [
            &mut self.bg_base,
            &mut self.bg_input,
            &mut self.selection_bg,
        ] {
            c.a *= opacity;
        }
        self
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
