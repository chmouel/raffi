use std::sync::LazyLock;

use iced::widget::span;
use regex::Regex;

static ANSI_SGR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[([0-9;]*)m").unwrap());

fn ansi_color(code: u8) -> Option<iced::Color> {
    match code {
        30 => Some(iced::Color::from_rgb(0.0, 0.0, 0.0)),
        31 => Some(iced::Color::from_rgb(0.8, 0.2, 0.2)),
        32 => Some(iced::Color::from_rgb(0.2, 0.8, 0.2)),
        33 => Some(iced::Color::from_rgb(0.8, 0.8, 0.2)),
        34 => Some(iced::Color::from_rgb(0.3, 0.3, 0.9)),
        35 => Some(iced::Color::from_rgb(0.8, 0.2, 0.8)),
        36 => Some(iced::Color::from_rgb(0.2, 0.8, 0.8)),
        37 => Some(iced::Color::from_rgb(0.9, 0.9, 0.9)),
        90 => Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        91 => Some(iced::Color::from_rgb(1.0, 0.3, 0.3)),
        92 => Some(iced::Color::from_rgb(0.3, 1.0, 0.3)),
        93 => Some(iced::Color::from_rgb(1.0, 1.0, 0.3)),
        94 => Some(iced::Color::from_rgb(0.5, 0.5, 1.0)),
        95 => Some(iced::Color::from_rgb(1.0, 0.3, 1.0)),
        96 => Some(iced::Color::from_rgb(0.3, 1.0, 1.0)),
        97 => Some(iced::Color::from_rgb(1.0, 1.0, 1.0)),
        _ => None,
    }
}

/// Colour of an xterm 256-colour palette index.
fn xterm_256(n: u8) -> iced::Color {
    match n {
        0..=7 => ansi_color(30 + n).unwrap_or(iced::Color::WHITE),
        8..=15 => ansi_color(90 + n - 8).unwrap_or(iced::Color::WHITE),
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            iced::Color::from_rgb8(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        232..=255 => {
            let v = 8 + (n - 232) * 10;
            iced::Color::from_rgb8(v, v, v)
        }
    }
}

/// SGR state applied to the text that follows it.
#[derive(Clone, Copy)]
struct Style {
    fg: iced::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl Style {
    fn reset(default_color: iced::Color) -> Self {
        Self {
            fg: default_color,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
        }
    }

    fn apply(&mut self, params: &str, default_color: iced::Color) {
        if params.is_empty() {
            *self = Self::reset(default_color);
            return;
        }
        let codes: Vec<u8> = params
            .split(';')
            .map(|part| part.parse().unwrap_or(u8::MAX))
            .collect();
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => *self = Self::reset(default_color),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                39 => self.fg = default_color,
                code @ (30..=37 | 90..=97) => {
                    if let Some(color) = ansi_color(code) {
                        self.fg = color;
                    }
                }
                // Extended colours: `5;n` (256 palette) or `2;r;g;b`. Their
                // arguments must be skipped, not read as SGR codes.
                code @ (38 | 48) => {
                    let (color, used) = match codes.get(i + 1) {
                        Some(5) => (codes.get(i + 2).map(|&n| xterm_256(n)), 2),
                        Some(2) => match codes.get(i + 2..i + 5) {
                            Some(&[r, g, b]) => (Some(iced::Color::from_rgb8(r, g, b)), 4),
                            _ => (None, 4),
                        },
                        _ => (None, 0),
                    };
                    if code == 38 {
                        if let Some(color) = color {
                            self.fg = color;
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn span<'a>(
        &self,
        content: String,
        font_size: f32,
        font: iced::Font,
    ) -> iced::widget::text::Span<'a, (), iced::Font> {
        let mut color = self.fg;
        if self.dim {
            color.a *= 0.6;
        }
        let mut sp = span(content).size(font_size).color(color);
        if self.underline {
            sp = sp.underline(true);
        }
        if self.bold || self.italic {
            sp = sp.font(iced::Font {
                weight: if self.bold {
                    iced::font::Weight::Bold
                } else {
                    font.weight
                },
                style: if self.italic {
                    iced::font::Style::Italic
                } else {
                    font.style
                },
                ..font
            });
        }
        sp
    }
}

/// Turn ANSI text into styled spans. `font` is the font the text is drawn
/// with, so bold and italic runs keep its family (monospace stays aligned).
/// Escape sequences other than SGR colours and styles are dropped, tabs are
/// expanded and carriage returns removed.
pub(crate) fn ansi_to_spans<'a>(
    s: &str,
    font_size: f32,
    default_color: iced::Color,
    font: iced::Font,
) -> Vec<iced::widget::text::Span<'a, (), iced::Font>> {
    let s = sanitize(s);
    let mut spans = Vec::new();
    let mut style = Style::reset(default_color);
    let mut last_end = 0;

    for cap in ANSI_SGR_RE.captures_iter(&s) {
        let m = cap.get(0).unwrap();
        let before = &s[last_end..m.start()];
        if !before.is_empty() {
            spans.push(style.span(before.to_owned(), font_size, font));
        }
        last_end = m.end();
        style.apply(&cap[1], default_color);
    }

    let tail = &s[last_end..];
    if !tail.is_empty() || spans.is_empty() {
        spans.push(style.span(tail.to_owned(), font_size, font));
    }

    spans
}

/// Escape sequences other than SGR: CSI, OSC (hyperlinks, titles), and the
/// two- and three-byte forms such as charset selection.
static ANSI_ESCAPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)?|[ -/][0-~]|[0-~])")
        .unwrap()
});

const TAB_WIDTH: usize = 8;

/// Keep only SGR sequences and printable text: other escapes and control
/// characters are dropped, `\r` removed and tabs expanded to the next stop.
fn sanitize(s: &str) -> String {
    if !s.contains(|c: char| c.is_control() && c != '\n') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut column = 0;
    let mut last_end = 0;
    fn push_text(text: &str, out: &mut String, column: &mut usize) {
        for c in text.chars() {
            match c {
                '\n' => {
                    out.push(c);
                    *column = 0;
                }
                '\t' => {
                    let spaces = TAB_WIDTH - *column % TAB_WIDTH;
                    out.extend(std::iter::repeat_n(' ', spaces));
                    *column += spaces;
                }
                c if c.is_control() => {}
                c => {
                    out.push(c);
                    *column += 1;
                }
            }
        }
    }
    for m in ANSI_ESCAPE_RE.find_iter(s) {
        push_text(&s[last_end..m.start()], &mut out, &mut column);
        let sequence = m.as_str();
        if ANSI_SGR_RE.is_match(sequence) && sequence.ends_with('m') {
            out.push_str(sequence);
        }
        last_end = m.end();
    }
    push_text(&s[last_end..], &mut out, &mut column);
    out
}

pub(crate) fn strip_ansi(s: &str) -> String {
    ANSI_SGR_RE.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::{ansi_to_spans, sanitize, xterm_256};

    fn colors(s: &str) -> Vec<iced::Color> {
        ansi_to_spans(s, 12.0, iced::Color::WHITE, iced::Font::MONOSPACE)
            .into_iter()
            .map(|span| span.color.unwrap_or(iced::Color::WHITE))
            .collect()
    }

    #[test]
    fn test_truecolor_arguments_are_not_read_as_codes() {
        // 32 inside the RGB triple must not turn the text green.
        assert_eq!(
            colors("\x1b[38;2;10;32;200mx"),
            vec![iced::Color::from_rgb8(10, 32, 200)]
        );
    }

    #[test]
    fn test_256_colour_background_and_default_reset() {
        assert_eq!(colors("\x1b[38;5;196mx"), vec![xterm_256(196)]);
        assert_eq!(xterm_256(196), iced::Color::from_rgb8(255, 0, 0));
        // Backgrounds are ignored, including their arguments.
        assert_eq!(colors("\x1b[48;2;31;32;33mx"), vec![iced::Color::WHITE]);
        assert_eq!(
            colors("\x1b[31ma\x1b[39mb"),
            vec![super::ansi_color(31).unwrap(), iced::Color::WHITE]
        );
    }

    #[test]
    fn test_sanitize_drops_non_sgr_escapes_and_expands_tabs() {
        let osc8 = "\x1b]8;;file:///tmp/a\x1b\\a\x1b]8;;\x1b\\";
        assert_eq!(sanitize(osc8), "a");
        assert_eq!(sanitize("\x1b[2K\x1b[1Gab\r\n\x1b(Bc"), "ab\nc");
        assert_eq!(sanitize("\x1b[31mab\tc\x1b[0m"), "\x1b[31mab      c\x1b[0m");
        assert_eq!(sanitize("x\ty\n\tz"), "x       y\n        z");
    }

    #[test]
    fn test_bold_and_italic_keep_the_base_font_family() {
        let spans = ansi_to_spans(
            "\x1b[1ma\x1b[22;3mb\x1b[23;2mc",
            12.0,
            iced::Color::WHITE,
            iced::Font::MONOSPACE,
        );
        let fonts: Vec<_> = spans.iter().map(|span| span.font).collect();
        assert_eq!(
            fonts[0].map(|f| (f.family, f.weight)),
            Some((iced::font::Family::Monospace, iced::font::Weight::Bold))
        );
        assert_eq!(
            fonts[1].map(|f| (f.family, f.weight, f.style)),
            Some((
                iced::font::Family::Monospace,
                iced::font::Weight::Normal,
                iced::font::Style::Italic
            ))
        );
        assert_eq!(fonts[2], None);
        assert!(spans[2].color.unwrap().a < 1.0);
    }
}
