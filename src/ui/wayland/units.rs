use super::preview::{PreviewBody, PreviewData};
use super::types::{InlineProvider, InlineResult, InlineRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dimension {
    Length,
    Mass,
    Volume,
    Temperature,
    Data,
    Time,
    Speed,
    Area,
}

impl Dimension {
    fn name(self) -> &'static str {
        match self {
            Self::Length => "Length",
            Self::Mass => "Mass",
            Self::Volume => "Volume",
            Self::Temperature => "Temperature",
            Self::Data => "Data",
            Self::Time => "Time",
            Self::Speed => "Speed",
            Self::Area => "Area",
        }
    }
}

/// A unit converts to its dimension's base unit as `(value + offset) * factor`.
#[derive(Debug, PartialEq)]
pub(super) struct Unit {
    pub dimension: Dimension,
    pub symbol: &'static str,
    factor: f64,
    offset: f64,
    /// Lowercase names matched case-insensitively.
    aliases: &'static [&'static str],
    /// Names matched exactly, for data units where `Mb` and `MB` differ.
    exact: &'static [&'static str],
}

impl Unit {
    fn to_base(&self, value: f64) -> f64 {
        (value + self.offset) * self.factor
    }

    fn base_to(&self, base: f64) -> f64 {
        let value = base / self.factor - self.offset;
        // Removing an offset leaves float noise where the answer is zero,
        // such as 32 °F in °C.
        if value.abs() < self.offset.abs() * 1e-12 {
            0.0
        } else {
            value
        }
    }
}

const fn unit(
    dimension: Dimension,
    symbol: &'static str,
    factor: f64,
    aliases: &'static [&'static str],
) -> Unit {
    Unit {
        dimension,
        symbol,
        factor,
        offset: 0.0,
        aliases,
        exact: &[],
    }
}

const fn data(
    symbol: &'static str,
    bytes: f64,
    exact: &'static [&'static str],
    aliases: &'static [&'static str],
) -> Unit {
    Unit {
        dimension: Dimension::Data,
        symbol,
        factor: bytes,
        offset: 0.0,
        aliases,
        exact,
    }
}

use Dimension::*;

/// Base units: metre, kilogram, litre, kelvin, byte, second, metre per
/// second and square metre. Cups, pints, quarts, gallons and fluid ounces
/// are US customary unless prefixed with `imp` or `uk`.
#[rustfmt::skip]
static UNITS: &[Unit] = &[
    unit(Length, "mm", 0.001, &["mm", "millimeter", "millimeters", "millimetre", "millimetres"]),
    unit(Length, "cm", 0.01, &["cm", "centimeter", "centimeters", "centimetre", "centimetres"]),
    unit(Length, "m", 1.0, &["m", "meter", "meters", "metre", "metres"]),
    unit(Length, "km", 1000.0, &["km", "kilometer", "kilometers", "kilometre", "kilometres"]),
    unit(Length, "in", 0.0254, &["in", "inch", "inches"]),
    unit(Length, "ft", 0.3048, &["ft", "foot", "feet"]),
    unit(Length, "yd", 0.9144, &["yd", "yard", "yards"]),
    unit(Length, "mi", 1609.344, &["mi", "mile", "miles"]),
    unit(Length, "nmi", 1852.0, &["nmi", "nautical mile", "nautical miles"]),
    unit(Mass, "mg", 1e-6, &["mg", "milligram", "milligrams"]),
    unit(Mass, "g", 0.001, &["g", "gram", "grams"]),
    unit(Mass, "kg", 1.0, &["kg", "kilo", "kilos", "kilogram", "kilograms"]),
    unit(Mass, "t", 1000.0, &["t", "tonne", "tonnes", "metric ton", "metric tons"]),
    unit(Mass, "oz", 0.028_349_523_125, &["oz", "ounce", "ounces"]),
    unit(Mass, "lb", 0.453_592_37, &["lb", "lbs", "pound", "pounds"]),
    unit(Mass, "st", 6.350_293_18, &["st", "stone", "stones"]),
    unit(Volume, "ml", 0.001, &["ml", "milliliter", "milliliters", "millilitre", "millilitres"]),
    unit(Volume, "cl", 0.01, &["cl", "centiliter", "centiliters", "centilitre", "centilitres"]),
    unit(Volume, "dl", 0.1, &["dl", "deciliter", "deciliters", "decilitre", "decilitres"]),
    unit(Volume, "l", 1.0, &["l", "liter", "liters", "litre", "litres"]),
    unit(Volume, "cm³", 0.001, &["cm3", "cm³", "cc"]),
    unit(Volume, "m³", 1000.0, &["m3", "m³", "cubic meter", "cubic meters", "cubic metre", "cubic metres"]),
    unit(Volume, "tsp", 0.004_928_921_593_75, &["tsp", "teaspoon", "teaspoons"]),
    unit(Volume, "tbsp", 0.014_786_764_781_25, &["tbsp", "tablespoon", "tablespoons"]),
    unit(Volume, "fl oz", 0.029_573_529_562_5, &["floz", "fl oz", "fluid ounce", "fluid ounces"]),
    unit(Volume, "cup", 0.236_588_236_5, &["cup", "cups"]),
    unit(Volume, "pt", 0.473_176_473, &["pt", "pint", "pints"]),
    unit(Volume, "qt", 0.946_352_946, &["qt", "quart", "quarts"]),
    unit(Volume, "gal", 3.785_411_784, &["gal", "gallon", "gallons"]),
    unit(Volume, "imp fl oz", 0.028_413_062_5, &["imp floz", "imp fl oz", "uk floz", "uk fl oz"]),
    unit(Volume, "imp pt", 0.568_261_25, &["imp pt", "imp pint", "imp pints", "uk pt", "uk pint", "uk pints"]),
    unit(Volume, "imp qt", 1.136_522_5, &["imp qt", "imp quart", "imp quarts", "uk qt", "uk quart", "uk quarts"]),
    unit(Volume, "imp gal", 4.546_09, &["imp gal", "imp gallon", "imp gallons", "uk gal", "uk gallon", "uk gallons"]),
    Unit {
        dimension: Temperature,
        symbol: "°C",
        factor: 1.0,
        offset: 273.15,
        aliases: &["c", "°c", "celsius"],
        exact: &[],
    },
    Unit {
        dimension: Temperature,
        symbol: "°F",
        factor: 5.0 / 9.0,
        offset: 459.67,
        aliases: &["f", "°f", "fahrenheit"],
        exact: &[],
    },
    unit(Temperature, "K", 1.0, &["k", "kelvin", "kelvins"]),
    data("bit", 0.125, &["b"], &["bit", "bits"]),
    data("B", 1.0, &["B"], &["byte", "bytes"]),
    data("kb", 125.0, &["kb", "Kb"], &["kilobit", "kilobits"]),
    data("kB", 1e3, &["kB", "KB"], &["kilobyte", "kilobytes"]),
    data("Mb", 1.25e5, &["Mb"], &["megabit", "megabits"]),
    data("MB", 1e6, &["MB"], &["megabyte", "megabytes"]),
    data("Gb", 1.25e8, &["Gb"], &["gigabit", "gigabits"]),
    data("GB", 1e9, &["GB"], &["gigabyte", "gigabytes"]),
    data("Tb", 1.25e11, &["Tb"], &["terabit", "terabits"]),
    data("TB", 1e12, &["TB"], &["terabyte", "terabytes"]),
    data("PB", 1e15, &["PB"], &["petabyte", "petabytes"]),
    data("Kib", 128.0, &["Kib"], &["kibibit", "kibibits"]),
    data("KiB", 1024.0, &["KiB"], &["kibibyte", "kibibytes"]),
    data("Mib", 131_072.0, &["Mib"], &["mebibit", "mebibits"]),
    data("MiB", 1_048_576.0, &["MiB"], &["mebibyte", "mebibytes"]),
    data("Gib", 134_217_728.0, &["Gib"], &["gibibit", "gibibits"]),
    data("GiB", 1_073_741_824.0, &["GiB"], &["gibibyte", "gibibytes"]),
    data("TiB", 1_099_511_627_776.0, &["TiB"], &["tebibyte", "tebibytes"]),
    unit(Time, "ms", 0.001, &["ms", "millisecond", "milliseconds"]),
    unit(Time, "s", 1.0, &["s", "sec", "secs", "second", "seconds"]),
    unit(Time, "min", 60.0, &["min", "mins", "minute", "minutes"]),
    unit(Time, "h", 3600.0, &["h", "hr", "hrs", "hour", "hours"]),
    unit(Time, "d", 86_400.0, &["d", "day", "days"]),
    unit(Time, "wk", 604_800.0, &["wk", "wks", "week", "weeks"]),
    unit(Time, "yr", 31_557_600.0, &["yr", "yrs", "year", "years"]),
    unit(Speed, "m/s", 1.0, &["m/s", "mps"]),
    unit(Speed, "km/h", 1.0 / 3.6, &["km/h", "kmh", "kph"]),
    unit(Speed, "mph", 0.447_04, &["mph", "mi/h"]),
    unit(Speed, "kn", 1852.0 / 3600.0, &["kn", "kt", "kts", "knot", "knots"]),
    unit(Speed, "ft/s", 0.3048, &["ft/s", "fps"]),
    unit(Area, "cm²", 1e-4, &["cm2", "cm²", "sq cm"]),
    unit(Area, "m²", 1.0, &["m2", "m²", "sq m", "sqm"]),
    unit(Area, "km²", 1e6, &["km2", "km²", "sq km"]),
    unit(Area, "ha", 1e4, &["ha", "hectare", "hectares"]),
    unit(Area, "ac", 4_046.856_422_4, &["ac", "acre", "acres"]),
    unit(Area, "in²", 0.000_645_16, &["in2", "in²", "sq in"]),
    unit(Area, "ft²", 0.092_903_04, &["ft2", "ft²", "sq ft", "sqft"]),
    unit(Area, "yd²", 0.836_127_36, &["yd2", "yd²", "sq yd"]),
    unit(Area, "mi²", 2_589_988.110_336, &["mi2", "mi²", "sq mi"]),
];

fn find_unit(name: &str) -> Option<&'static Unit> {
    let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(unit) = UNITS
        .iter()
        .find(|unit| unit.exact.contains(&normalized.as_str()))
    {
        return Some(unit);
    }
    let lower = normalized.to_lowercase();
    UNITS
        .iter()
        .find(|unit| unit.aliases.contains(&lower.as_str()))
}

#[derive(Debug, PartialEq)]
pub(super) struct UnitConversion {
    pub amount: f64,
    pub from: &'static Unit,
    pub to: &'static Unit,
    pub value: f64,
}

/// Splits a leading number such as `10`, `-4.5` or `.5` off `text`.
fn split_amount(text: &str) -> Option<(f64, &str)> {
    let end = text
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && matches!(c, '-' | '+'))))
        .map_or(text.len(), |(i, _)| i);
    let (number, rest) = text.split_at(end);
    if !number.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((number.parse().ok()?, rest.trim()))
}

/// Parses `<number> <unit> to|in <unit>`, such as `10 km to mi` or `70f in c`.
/// A unit is required on both sides and both must share a dimension.
pub(super) fn try_parse_unit_conversion(query: &str) -> Option<UnitConversion> {
    let trimmed = query.trim();
    if !trimmed.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '.' | '-' | '+')) {
        return None;
    }
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    // `in` is both a separator and the inch symbol, so try every split.
    (1..words.len().saturating_sub(1))
        .filter(|&i| words[i].eq_ignore_ascii_case("to") || words[i].eq_ignore_ascii_case("in"))
        .find_map(|i| {
            let (amount, from) = split_amount(&words[..i].join(" "))
                .and_then(|(amount, name)| Some((amount, find_unit(name)?)))?;
            let to = find_unit(&words[i + 1..].join(" "))?;
            if from.dimension != to.dimension {
                return None;
            }
            let value = to.base_to(from.to_base(amount));
            value.is_finite().then_some(UnitConversion {
                amount,
                from,
                to,
                value,
            })
        })
}

/// `value` rounded to `digits` significant digits, without a `-0`.
fn format_significant(value: f64, digits: usize) -> String {
    let rounded = format!("{value:.*e}", digits - 1)
        .parse()
        .ok()
        .filter(|rounded: &f64| rounded.is_finite())
        .unwrap_or(value);
    if rounded == 0.0 {
        "0".to_string()
    } else {
        rounded.to_string()
    }
}

/// Whole numbers f64 holds exactly are copied as is; anything else is
/// rounded to 15 significant digits to drop float noise such as
/// 1 ft = 12.000000000000002 in.
fn copy_value(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 2f64.powi(53) {
        format_significant(value, 17)
    } else {
        format_significant(value, 15)
    }
}

pub(super) fn units_inline_result(conversion: &UnitConversion) -> InlineResult {
    let from = format!("{} {}", conversion.amount, conversion.from.symbol);
    let shown = format_significant(conversion.value, 6);
    let exact = copy_value(conversion.value);
    let with_unit = format!("{exact} {}", conversion.to.symbol);
    let dimension = conversion.from.dimension.name();
    let preview = PreviewData {
        title: "Unit conversion".to_string(),
        subtitle: None,
        badge: Some(dimension.to_string()),
        icon: None,
        bodies: vec![PreviewBody::HeroValue {
            expression: Some(from.clone()),
            value: format!("{shown} {}", conversion.to.symbol),
        }],
        metadata: vec![
            ("Exact".to_string(), with_unit.clone()),
            ("Enter".to_string(), format!("Copy {exact} to clipboard")),
            (
                "Ctrl+Enter".to_string(),
                format!("Copy {with_unit} to clipboard"),
            ),
        ],
    };
    InlineResult {
        provider: InlineProvider::Units,
        rows: vec![InlineRow {
            title: format!("{from} = {shown} {}", conversion.to.symbol),
            subtitle: Some(dimension.to_string()),
            copy_value: exact,
            alt_copy_value: Some(with_unit),
            swatch: None,
            preview: Some(preview),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(query: &str) -> Option<(f64, &'static str)> {
        try_parse_unit_conversion(query).map(|c| (c.value, c.to.symbol))
    }

    fn assert_converts(query: &str, expected: f64, symbol: &str) {
        let (value, to) = convert(query).unwrap_or_else(|| panic!("{query} did not parse"));
        assert_eq!(to, symbol, "{query}");
        assert!(
            (value - expected).abs() <= expected.abs() * 1e-9,
            "{query}: {value} != {expected}"
        );
    }

    #[test]
    fn test_aliases_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for unit in UNITS {
            for alias in unit.aliases {
                assert_eq!(*alias, alias.to_lowercase(), "{alias}");
                assert!(seen.insert(*alias), "duplicate alias {alias}");
            }
        }
        let mut exact = std::collections::HashSet::new();
        for unit in UNITS {
            for name in unit.exact {
                assert!(exact.insert(*name), "duplicate exact name {name}");
            }
        }
    }

    #[test]
    fn test_conversions_per_dimension() {
        assert_converts("10 km to mi", 6.213_711_922_373_34, "mi");
        assert_converts("10km in miles", 6.213_711_922_373_34, "mi");
        assert_converts("12 in to cm", 30.48, "cm");
        assert_converts("5 in in cm", 12.7, "cm");
        assert_converts("1 ft to in", 12.0, "in");
        assert_converts("1 kg to lb", 2.204_622_621_848_78, "lb");
        assert_converts("3 cups to ml", 709.764_709_5, "ml");
        assert_converts("1 gal to l", 3.785_411_784, "l");
        assert_converts("1 imp gal to l", 4.546_09, "l");
        assert_converts("1 UK gal to gal", 1.200_949_925_504_24, "gal");
        assert_converts("16 fl oz to cup", 2.0, "cup");
        assert_converts("70f to c", 21.111_111_111_111_1, "°C");
        assert_converts("-40 C to F", -40.0, "°F");
        assert_converts("0 k to celsius", -273.15, "°C");
        assert_converts("5 GB in MB", 5000.0, "MB");
        assert_converts("1 KiB to B", 1024.0, "B");
        assert_converts("1 KB to B", 1000.0, "B");
        assert_converts("1 kB to B", 1000.0, "B");
        assert_converts("100 Mb to MB", 12.5, "MB");
        assert_converts("1 GiB to megabytes", 1_073.741_824, "MB");
        assert_converts("1.5 hours to min", 90.0, "min");
        assert_converts("1 day to s", 86_400.0, "s");
        assert_converts("100 km/h to mph", 62.137_119_223_733_4, "mph");
        assert_converts("10 m/s to km/h", 36.0, "km/h");
        assert_converts("1 acre to m2", 4_046.856_422_4, "m²");
        assert_converts("1 sq mi to km2", 2.589_988_110_336, "km²");
        assert_converts(".5 l to ml", 500.0, "ml");
    }

    #[test]
    fn test_data_units_are_case_sensitive() {
        assert!(convert("5 gb to mb").is_none());
        assert!(convert("5 Mb to mB").is_none());
        assert_converts("8 b to B", 1.0, "B");
        assert_converts("8 bits to bytes", 1.0, "B");
    }

    #[test]
    fn test_non_matching_inputs() {
        for query in [
            "",
            "firefox",
            "10 apples",
            "10 apples to kg",
            "10 to",
            "10 km to",
            "5 m",
            "70f",
            "km to mi",
            "10 km mi",
            "10 km to kg",
            "10 c to m",
            "10 usd to eur",
            "$10 usd to eur",
            "1+1",
            "1e999 km to m",
            "to mi",
            "- km to mi",
        ] {
            assert!(convert(query).is_none(), "{query} should not match");
        }
    }

    #[test]
    fn test_infinite_results_are_rejected() {
        let huge = format!("{} km to mm", "9".repeat(306));
        assert!(convert(&huge).is_none());
    }

    #[test]
    fn test_inline_row_rounds_display_and_keeps_precision_for_copy() {
        let conversion = try_parse_unit_conversion("10 km to mi").unwrap();
        let inline = units_inline_result(&conversion);
        assert_eq!(inline.provider, InlineProvider::Units);
        let row = &inline.rows[0];
        assert_eq!(row.title, "10 km = 6.21371 mi");
        assert_eq!(row.subtitle.as_deref(), Some("Length"));
        assert_eq!(row.copy_value, "6.21371192237334");
        assert_eq!(row.alt_copy_value.as_deref(), Some("6.21371192237334 mi"));

        let row = &units_inline_result(&try_parse_unit_conversion("1 ft to in").unwrap()).rows[0];
        assert_eq!(row.copy_value, "12");
        let row = &units_inline_result(&try_parse_unit_conversion("32 f to c").unwrap()).rows[0];
        assert_eq!(row.title, "32 °F = 0 °C");
        assert_eq!(row.copy_value, "0");
    }

    #[test]
    fn test_copy_value_keeps_exact_integers_and_stays_finite() {
        assert_eq!(copy_value(1_234_567_890_123_456.0), "1234567890123456");
        assert_eq!(copy_value(12.000_000_000_000_002), "12");
        assert_eq!(copy_value(f64::MAX), f64::MAX.to_string());
        assert!(!format_significant(f64::MAX, 1).contains("inf"));
        let row =
            &units_inline_result(&try_parse_unit_conversion("1234567890123456 B to B").unwrap())
                .rows[0];
        assert_eq!(row.copy_value, "1234567890123456");
    }
}
