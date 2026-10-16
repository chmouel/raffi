use super::preview::{PreviewBody, PreviewData};
use super::types::{InlineProvider, InlineResult, InlineRow};

/// A parsed color; `alpha` is in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub alpha: f64,
}

/// The CSS Color Level 4 named colors, sorted so duplicates such as
/// `aqua`/`cyan` resolve to the first name.
const CSS_COLORS: [(&str, u8, u8, u8); 148] = [
    ("aliceblue", 0xf0, 0xf8, 0xff),
    ("antiquewhite", 0xfa, 0xeb, 0xd7),
    ("aqua", 0x00, 0xff, 0xff),
    ("aquamarine", 0x7f, 0xff, 0xd4),
    ("azure", 0xf0, 0xff, 0xff),
    ("beige", 0xf5, 0xf5, 0xdc),
    ("bisque", 0xff, 0xe4, 0xc4),
    ("black", 0x00, 0x00, 0x00),
    ("blanchedalmond", 0xff, 0xeb, 0xcd),
    ("blue", 0x00, 0x00, 0xff),
    ("blueviolet", 0x8a, 0x2b, 0xe2),
    ("brown", 0xa5, 0x2a, 0x2a),
    ("burlywood", 0xde, 0xb8, 0x87),
    ("cadetblue", 0x5f, 0x9e, 0xa0),
    ("chartreuse", 0x7f, 0xff, 0x00),
    ("chocolate", 0xd2, 0x69, 0x1e),
    ("coral", 0xff, 0x7f, 0x50),
    ("cornflowerblue", 0x64, 0x95, 0xed),
    ("cornsilk", 0xff, 0xf8, 0xdc),
    ("crimson", 0xdc, 0x14, 0x3c),
    ("cyan", 0x00, 0xff, 0xff),
    ("darkblue", 0x00, 0x00, 0x8b),
    ("darkcyan", 0x00, 0x8b, 0x8b),
    ("darkgoldenrod", 0xb8, 0x86, 0x0b),
    ("darkgray", 0xa9, 0xa9, 0xa9),
    ("darkgreen", 0x00, 0x64, 0x00),
    ("darkgrey", 0xa9, 0xa9, 0xa9),
    ("darkkhaki", 0xbd, 0xb7, 0x6b),
    ("darkmagenta", 0x8b, 0x00, 0x8b),
    ("darkolivegreen", 0x55, 0x6b, 0x2f),
    ("darkorange", 0xff, 0x8c, 0x00),
    ("darkorchid", 0x99, 0x32, 0xcc),
    ("darkred", 0x8b, 0x00, 0x00),
    ("darksalmon", 0xe9, 0x96, 0x7a),
    ("darkseagreen", 0x8f, 0xbc, 0x8f),
    ("darkslateblue", 0x48, 0x3d, 0x8b),
    ("darkslategray", 0x2f, 0x4f, 0x4f),
    ("darkslategrey", 0x2f, 0x4f, 0x4f),
    ("darkturquoise", 0x00, 0xce, 0xd1),
    ("darkviolet", 0x94, 0x00, 0xd3),
    ("deeppink", 0xff, 0x14, 0x93),
    ("deepskyblue", 0x00, 0xbf, 0xff),
    ("dimgray", 0x69, 0x69, 0x69),
    ("dimgrey", 0x69, 0x69, 0x69),
    ("dodgerblue", 0x1e, 0x90, 0xff),
    ("firebrick", 0xb2, 0x22, 0x22),
    ("floralwhite", 0xff, 0xfa, 0xf0),
    ("forestgreen", 0x22, 0x8b, 0x22),
    ("fuchsia", 0xff, 0x00, 0xff),
    ("gainsboro", 0xdc, 0xdc, 0xdc),
    ("ghostwhite", 0xf8, 0xf8, 0xff),
    ("gold", 0xff, 0xd7, 0x00),
    ("goldenrod", 0xda, 0xa5, 0x20),
    ("gray", 0x80, 0x80, 0x80),
    ("green", 0x00, 0x80, 0x00),
    ("greenyellow", 0xad, 0xff, 0x2f),
    ("grey", 0x80, 0x80, 0x80),
    ("honeydew", 0xf0, 0xff, 0xf0),
    ("hotpink", 0xff, 0x69, 0xb4),
    ("indianred", 0xcd, 0x5c, 0x5c),
    ("indigo", 0x4b, 0x00, 0x82),
    ("ivory", 0xff, 0xff, 0xf0),
    ("khaki", 0xf0, 0xe6, 0x8c),
    ("lavender", 0xe6, 0xe6, 0xfa),
    ("lavenderblush", 0xff, 0xf0, 0xf5),
    ("lawngreen", 0x7c, 0xfc, 0x00),
    ("lemonchiffon", 0xff, 0xfa, 0xcd),
    ("lightblue", 0xad, 0xd8, 0xe6),
    ("lightcoral", 0xf0, 0x80, 0x80),
    ("lightcyan", 0xe0, 0xff, 0xff),
    ("lightgoldenrodyellow", 0xfa, 0xfa, 0xd2),
    ("lightgray", 0xd3, 0xd3, 0xd3),
    ("lightgreen", 0x90, 0xee, 0x90),
    ("lightgrey", 0xd3, 0xd3, 0xd3),
    ("lightpink", 0xff, 0xb6, 0xc1),
    ("lightsalmon", 0xff, 0xa0, 0x7a),
    ("lightseagreen", 0x20, 0xb2, 0xaa),
    ("lightskyblue", 0x87, 0xce, 0xfa),
    ("lightslategray", 0x77, 0x88, 0x99),
    ("lightslategrey", 0x77, 0x88, 0x99),
    ("lightsteelblue", 0xb0, 0xc4, 0xde),
    ("lightyellow", 0xff, 0xff, 0xe0),
    ("lime", 0x00, 0xff, 0x00),
    ("limegreen", 0x32, 0xcd, 0x32),
    ("linen", 0xfa, 0xf0, 0xe6),
    ("magenta", 0xff, 0x00, 0xff),
    ("maroon", 0x80, 0x00, 0x00),
    ("mediumaquamarine", 0x66, 0xcd, 0xaa),
    ("mediumblue", 0x00, 0x00, 0xcd),
    ("mediumorchid", 0xba, 0x55, 0xd3),
    ("mediumpurple", 0x93, 0x70, 0xdb),
    ("mediumseagreen", 0x3c, 0xb3, 0x71),
    ("mediumslateblue", 0x7b, 0x68, 0xee),
    ("mediumspringgreen", 0x00, 0xfa, 0x9a),
    ("mediumturquoise", 0x48, 0xd1, 0xcc),
    ("mediumvioletred", 0xc7, 0x15, 0x85),
    ("midnightblue", 0x19, 0x19, 0x70),
    ("mintcream", 0xf5, 0xff, 0xfa),
    ("mistyrose", 0xff, 0xe4, 0xe1),
    ("moccasin", 0xff, 0xe4, 0xb5),
    ("navajowhite", 0xff, 0xde, 0xad),
    ("navy", 0x00, 0x00, 0x80),
    ("oldlace", 0xfd, 0xf5, 0xe6),
    ("olive", 0x80, 0x80, 0x00),
    ("olivedrab", 0x6b, 0x8e, 0x23),
    ("orange", 0xff, 0xa5, 0x00),
    ("orangered", 0xff, 0x45, 0x00),
    ("orchid", 0xda, 0x70, 0xd6),
    ("palegoldenrod", 0xee, 0xe8, 0xaa),
    ("palegreen", 0x98, 0xfb, 0x98),
    ("paleturquoise", 0xaf, 0xee, 0xee),
    ("palevioletred", 0xdb, 0x70, 0x93),
    ("papayawhip", 0xff, 0xef, 0xd5),
    ("peachpuff", 0xff, 0xda, 0xb9),
    ("peru", 0xcd, 0x85, 0x3f),
    ("pink", 0xff, 0xc0, 0xcb),
    ("plum", 0xdd, 0xa0, 0xdd),
    ("powderblue", 0xb0, 0xe0, 0xe6),
    ("purple", 0x80, 0x00, 0x80),
    ("rebeccapurple", 0x66, 0x33, 0x99),
    ("red", 0xff, 0x00, 0x00),
    ("rosybrown", 0xbc, 0x8f, 0x8f),
    ("royalblue", 0x41, 0x69, 0xe1),
    ("saddlebrown", 0x8b, 0x45, 0x13),
    ("salmon", 0xfa, 0x80, 0x72),
    ("sandybrown", 0xf4, 0xa4, 0x60),
    ("seagreen", 0x2e, 0x8b, 0x57),
    ("seashell", 0xff, 0xf5, 0xee),
    ("sienna", 0xa0, 0x52, 0x2d),
    ("silver", 0xc0, 0xc0, 0xc0),
    ("skyblue", 0x87, 0xce, 0xeb),
    ("slateblue", 0x6a, 0x5a, 0xcd),
    ("slategray", 0x70, 0x80, 0x90),
    ("slategrey", 0x70, 0x80, 0x90),
    ("snow", 0xff, 0xfa, 0xfa),
    ("springgreen", 0x00, 0xff, 0x7f),
    ("steelblue", 0x46, 0x82, 0xb4),
    ("tan", 0xd2, 0xb4, 0x8c),
    ("teal", 0x00, 0x80, 0x80),
    ("thistle", 0xd8, 0xbf, 0xd8),
    ("tomato", 0xff, 0x63, 0x47),
    ("turquoise", 0x40, 0xe0, 0xd0),
    ("violet", 0xee, 0x82, 0xee),
    ("wheat", 0xf5, 0xde, 0xb3),
    ("white", 0xff, 0xff, 0xff),
    ("whitesmoke", 0xf5, 0xf5, 0xf5),
    ("yellow", 0xff, 0xff, 0x00),
    ("yellowgreen", 0x9a, 0xcd, 0x32),
];

/// Parses `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`/`rgba()` and
/// `hsl()`/`hsla()` with comma or space separated values.
pub(super) fn try_parse_color(query: &str) -> Option<Rgba> {
    let query = query.trim().to_ascii_lowercase();
    if let Some(hex) = query.strip_prefix('#') {
        return parse_hex(hex);
    }
    let (name, args) = query.strip_suffix(')')?.split_once('(')?;
    let args = parse_args(args)?;
    match name {
        "rgb" | "rgba" => parse_rgb(&args),
        "hsl" | "hsla" => parse_hsl(&args),
        _ => None,
    }
}

fn parse_hex(hex: &str) -> Option<Rgba> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digits: Vec<u8> = match hex.len() {
        3 | 4 => hex
            .chars()
            .map(|c| u8::from_str_radix(&format!("{c}{c}"), 16).ok())
            .collect::<Option<_>>()?,
        6 | 8 => (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect::<Option<_>>()?,
        _ => return None,
    };
    Some(Rgba {
        r: digits[0],
        g: digits[1],
        b: digits[2],
        alpha: digits.get(3).map_or(1.0, |&a| f64::from(a) / 255.0),
    })
}

/// Splits `a, b, c[, d]` or `a b c[ / d]` into three or four values.
fn parse_args(args: &str) -> Option<Vec<&str>> {
    let values: Vec<&str> = if args.contains(',') {
        args.split(',').map(str::trim).collect()
    } else {
        let (channels, alpha) = match args.split_once('/') {
            Some((channels, alpha)) => (channels, Some(alpha.trim())),
            None => (args, None),
        };
        let mut values: Vec<&str> = channels.split_whitespace().collect();
        if values.len() != 3 {
            return None;
        }
        values.extend(alpha);
        values
    };
    (matches!(values.len(), 3 | 4) && values.iter().all(|v| !v.is_empty())).then_some(values)
}

fn parse_number(value: &str) -> Option<f64> {
    if !value
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return None;
    }
    value.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Parses `value` (or `value%` scaled to `max`) within `0..=max`.
fn parse_in_range(value: &str, max: f64) -> Option<f64> {
    let number = match value.strip_suffix('%') {
        Some(percent) => parse_number(percent).filter(|p| (0.0..=100.0).contains(p))? * max / 100.0,
        None => parse_number(value)?,
    };
    (0.0..=max).contains(&number).then_some(number)
}

fn parse_alpha(args: &[&str]) -> Option<f64> {
    args.get(3)
        .map_or(Some(1.0), |alpha| parse_in_range(alpha, 1.0))
}

fn parse_rgb(args: &[&str]) -> Option<Rgba> {
    let channel = |value: &str| parse_in_range(value, 255.0).map(|n| n.round() as u8);
    Some(Rgba {
        r: channel(args[0])?,
        g: channel(args[1])?,
        b: channel(args[2])?,
        alpha: parse_alpha(args)?,
    })
}

fn parse_hsl(args: &[&str]) -> Option<Rgba> {
    let hue = parse_number(args[0].strip_suffix("deg").unwrap_or(args[0]))
        .filter(|h| (0.0..=360.0).contains(h))?;
    let percent = |value: &str| {
        parse_number(value.strip_suffix('%').unwrap_or(value)).filter(|p| (0.0..=100.0).contains(p))
    };
    let saturation = percent(args[1])?;
    let lightness = percent(args[2])?;
    let (r, g, b) = hsl_to_rgb(hue, saturation / 100.0, lightness / 100.0);
    Some(Rgba {
        r,
        g,
        b,
        alpha: parse_alpha(args)?,
    })
}

fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = (hue % 360.0) / 60.0;
    let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u8 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    let to_byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to_byte(r), to_byte(g), to_byte(b))
}

/// Returns hue in degrees and saturation and lightness in percent.
fn rgb_to_hsl(color: &Rgba) -> (f64, f64, f64) {
    let [r, g, b] = [color.r, color.g, color.b].map(|c| f64::from(c) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta == 0.0 {
        return (0.0, 0.0, lightness * 100.0);
    }
    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (hue, saturation * 100.0, lightness * 100.0)
}

/// The closest CSS named color by RGB distance, and whether it is exact.
fn nearest_css_color(color: &Rgba) -> (&'static str, Rgba, bool) {
    let distance = |&(_, r, g, b): &(&str, u8, u8, u8)| {
        [(r, color.r), (g, color.g), (b, color.b)]
            .iter()
            .map(|&(a, b)| (i32::from(a) - i32::from(b)).pow(2))
            .sum::<i32>()
    };
    let nearest = CSS_COLORS
        .iter()
        .min_by_key(|entry| distance(entry))
        .expect("color table is not empty");
    let (name, r, g, b) = *nearest;
    let exact = distance(nearest) == 0 && color.alpha == 1.0;
    (
        name,
        Rgba {
            r,
            g,
            b,
            alpha: 1.0,
        },
        exact,
    )
}

fn format_decimal(value: f64, places: usize) -> String {
    let text = format!("{value:.places$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    if text == "-0" {
        "0".to_string()
    } else {
        text.to_string()
    }
}

fn format_alpha(alpha: f64) -> String {
    format_decimal(alpha, 3)
}

fn to_hex(color: &Rgba) -> String {
    let rgb = format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b);
    if color.alpha < 1.0 {
        format!("{rgb}{:02x}", (color.alpha * 255.0).round() as u8)
    } else {
        rgb
    }
}

fn to_rgb(color: &Rgba) -> String {
    if color.alpha < 1.0 {
        format!(
            "rgba({}, {}, {}, {})",
            color.r,
            color.g,
            color.b,
            format_alpha(color.alpha)
        )
    } else {
        format!("rgb({}, {}, {})", color.r, color.g, color.b)
    }
}

fn to_hsl(color: &Rgba) -> String {
    let (h, s, l) = rgb_to_hsl(color);
    // One decimal keeps every 8-bit color exact when converted back.
    let h = if format_decimal(h, 1) == "360" {
        0.0
    } else {
        h
    };
    let [h, s, l] = [h, s, l].map(|v| format_decimal(v, 1));
    if color.alpha < 1.0 {
        format!("hsla({h}, {s}%, {l}%, {})", format_alpha(color.alpha))
    } else {
        format!("hsl({h}, {s}%, {l}%)")
    }
}

fn swatch(color: &Rgba) -> iced::Color {
    iced::Color::from_rgba8(color.r, color.g, color.b, color.alpha as f32)
}

pub(super) fn color_inline_result(color: &Rgba) -> InlineResult {
    let hex = to_hex(color);
    let rgb = to_rgb(color);
    let hsl = to_hsl(color);
    let (name, named, exact) = nearest_css_color(color);
    let (name_title, name_subtitle) = if exact {
        (name.to_string(), "CSS name, exact match")
    } else {
        (format!("nearest: {name}"), "CSS name")
    };
    let metadata = vec![
        ("Hex".to_string(), hex.clone()),
        ("RGB".to_string(), rgb.clone()),
        ("HSL".to_string(), hsl.clone()),
        ("CSS name".to_string(), name_title.clone()),
        ("Enter".to_string(), "Copy the selected format".to_string()),
    ];
    let row = |title: String, subtitle: &str, copy_value: String, swatch_color: &Rgba| {
        let preview = PreviewData {
            title: "Color".to_string(),
            subtitle: None,
            badge: Some(subtitle.to_string()),
            icon: None,
            bodies: vec![PreviewBody::HeroValue {
                expression: None,
                value: title.clone(),
            }],
            metadata: metadata.clone(),
        };
        InlineRow {
            title,
            subtitle: Some(subtitle.to_string()),
            copy_value,
            alt_copy_value: None,
            swatch: Some(swatch(swatch_color)),
            preview: Some(preview),
        }
    };
    InlineResult {
        provider: InlineProvider::Color,
        rows: vec![
            row(hex.clone(), "Hex", hex, color),
            row(rgb.clone(), "RGB", rgb, color),
            row(hsl.clone(), "HSL", hsl, color),
            row(name_title, name_subtitle, name.to_string(), &named),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formats(query: &str) -> Vec<String> {
        let color = try_parse_color(query).unwrap_or_else(|| panic!("{query} should parse"));
        color_inline_result(&color)
            .rows
            .into_iter()
            .map(|row| row.title)
            .collect()
    }

    #[test]
    fn test_parses_every_supported_syntax_to_the_same_color() {
        let orange = vec![
            "#ff8800".to_string(),
            "rgb(255, 136, 0)".to_string(),
            "hsl(32, 100%, 50%)".to_string(),
            "nearest: darkorange".to_string(),
        ];
        for query in [
            "#ff8800",
            "#FF8800",
            "#f80",
            " #f80 ",
            "rgb(255,136,0)",
            "RGB(255 136 0)",
            "rgba(255, 136, 0, 1)",
            "rgb(100% 53.4% 0%)",
            "hsl(32,100%,50%)",
            "hsl(32 100% 50%)",
            "hsla(32deg, 100%, 50%, 100%)",
        ] {
            assert_eq!(formats(query), orange, "{query}");
        }
    }

    #[test]
    fn test_alpha_and_exact_names() {
        assert_eq!(
            formats("#ff000080"),
            [
                "#ff000080",
                "rgba(255, 0, 0, 0.502)",
                "hsla(0, 100%, 50%, 0.502)",
                "nearest: red",
            ]
        );
        assert_eq!(
            formats("rgb(255 0 0 / 0.5)")[1],
            "rgba(255, 0, 0, 0.5)".to_string()
        );
        assert_eq!(formats("#f008")[0], "#ff000088");
        let exact = color_inline_result(&try_parse_color("#663399").unwrap());
        assert_eq!(exact.rows[3].title, "rebeccapurple");
        assert_eq!(exact.rows[3].copy_value, "rebeccapurple");
        assert_eq!(
            exact.rows[3].subtitle.as_deref(),
            Some("CSS name, exact match")
        );
        assert_eq!(formats("#00ffff")[3], "aqua");
        assert_eq!(formats("#808080")[2], "hsl(0, 0%, 50.2%)");
        assert_eq!(formats("hsl(360, 100%, 50%)")[0], "#ff0000");
    }

    #[test]
    fn test_rows_copy_their_format_and_carry_swatches() {
        let result = color_inline_result(&try_parse_color("#123").unwrap());
        assert_eq!(result.provider, InlineProvider::Color);
        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.rows[0].copy_value, "#112233");
        assert_eq!(result.rows[1].copy_value, "rgb(17, 34, 51)");
        assert!(result.rows.iter().all(|row| row.swatch.is_some()));
        let named = CSS_COLORS
            .iter()
            .find(|(name, ..)| result.rows[3].copy_value == *name)
            .unwrap();
        assert_eq!(
            result.rows[3].swatch,
            Some(iced::Color::from_rgb8(named.1, named.2, named.3))
        );
    }

    #[test]
    fn test_rejects_invalid_or_out_of_range_input() {
        for query in [
            "",
            "#",
            "#zzz",
            "#12",
            "#12345",
            "#1234567",
            "# fff",
            "red",
            "firefox",
            "color picker",
            "rgb",
            "rgb()",
            "rgb(256, 0, 0)",
            "rgb(-1, 0, 0)",
            "rgb(1, 2)",
            "rgb(1, 2, 3, 4, 5)",
            "rgb(1 2 3 4)",
            "rgb(1, 2 3)",
            "rgb(1, 2, 3, 1.5)",
            "rgb(101%, 0, 0)",
            "rgb(nan, 0, 0)",
            "rgb(inf, 0, 0)",
            "rgb(1e2, 0, 0)",
            "rgb(1, 2, 3",
            "hsl(361, 50%, 50%)",
            "hsl(30, 101%, 50%)",
            "hsl(30, 50%, -1%)",
            "cmyk(0, 0, 0, 0)",
        ] {
            assert_eq!(try_parse_color(query), None, "{query}");
        }
    }

    #[test]
    fn test_hsl_output_round_trips_to_the_same_color() {
        for r in (0..=255u8).step_by(3) {
            for g in (0..=255u8).step_by(5) {
                for b in (0..=255u8).step_by(7) {
                    let color = Rgba {
                        r,
                        g,
                        b,
                        alpha: 1.0,
                    };
                    let hsl = to_hsl(&color);
                    assert_eq!(try_parse_color(&hsl), Some(color), "{hsl}");
                }
            }
        }
        for hex in ["#010101", "#fefefe", "#123456", "#000001", "#fffffe"] {
            let color = try_parse_color(hex).unwrap();
            assert_eq!(try_parse_color(&to_hsl(&color)), Some(color), "{hex}");
        }
        assert_eq!(formats("#123456")[2], "hsl(210, 65.4%, 20.4%)");
    }

    #[test]
    fn test_named_color_table() {
        assert_eq!(CSS_COLORS.len(), 148);
        assert!(CSS_COLORS.windows(2).all(|w| w[0].0 < w[1].0));
    }
}
