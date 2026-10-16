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

pub(crate) fn ansi_to_spans<'a>(
    s: &str,
    font_size: f32,
    default_color: iced::Color,
) -> Vec<iced::widget::text::Span<'a, (), iced::Font>> {
    let mut spans = Vec::new();
    let mut fg = default_color;
    let mut bold = false;
    let mut underline = false;
    let mut last_end = 0;

    for cap in ANSI_SGR_RE.captures_iter(s) {
        let m = cap.get(0).unwrap();
        let before = &s[last_end..m.start()];
        if !before.is_empty() {
            let mut sp = span(before.to_owned()).size(font_size).color(fg);
            if underline {
                sp = sp.underline(true);
            }
            if bold {
                sp = sp.font(iced::Font {
                    weight: iced::font::Weight::Bold,
                    ..iced::Font::default()
                });
            }
            spans.push(sp);
        }
        last_end = m.end();

        let params = &cap[1];
        if params.is_empty() {
            fg = default_color;
            bold = false;
            underline = false;
            continue;
        }

        let codes: Vec<u8> = params
            .split(';')
            .map(|part| part.parse().unwrap_or(u8::MAX))
            .collect();
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => {
                    fg = default_color;
                    bold = false;
                    underline = false;
                }
                1 => bold = true,
                4 => underline = true,
                22 => bold = false,
                24 => underline = false,
                39 => fg = default_color,
                code @ (30..=37 | 90..=97) => {
                    if let Some(color) = ansi_color(code) {
                        fg = color;
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
                            fg = color;
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }

    let tail = &s[last_end..];
    if !tail.is_empty() || spans.is_empty() {
        let mut sp = span(tail.to_owned()).size(font_size).color(fg);
        if underline {
            sp = sp.underline(true);
        }
        if bold {
            sp = sp.font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::default()
            });
        }
        spans.push(sp);
    }

    spans
}

pub(crate) fn strip_ansi(s: &str) -> String {
    ANSI_SGR_RE.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::{ansi_to_spans, xterm_256};

    fn colors(s: &str) -> Vec<iced::Color> {
        ansi_to_spans(s, 12.0, iced::Color::WHITE)
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
}
