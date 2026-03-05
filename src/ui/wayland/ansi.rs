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

        for part in params.split(';') {
            if let Ok(code) = part.parse::<u8>() {
                match code {
                    0 => {
                        fg = default_color;
                        bold = false;
                        underline = false;
                    }
                    1 => bold = true,
                    4 => underline = true,
                    22 => bold = false,
                    24 => underline = false,
                    30..=37 | 90..=97 => {
                        if let Some(color) = ansi_color(code) {
                            fg = color;
                        }
                    }
                    _ => {}
                }
            }
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
