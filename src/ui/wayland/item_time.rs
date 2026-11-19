//! The `time` field of a script filter item: a moment the script gives as data,
//! so Raffi can show how long ago it was whenever the row is drawn instead of
//! the script baking a text that goes stale in the cache.

use serde::de::{Deserialize, Deserializer};
use serde_json::value::RawValue;

/// Longest label kept, in characters.
const MAX_LABEL_CHARS: usize = 32;
/// Furthest from now a time may lie, in seconds, about a century: beyond it
/// the arithmetic is not worth the trouble and the value is surely wrong.
const MAX_DISTANCE: i64 = 100 * 365 * 24 * 3600;

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// A moment, in Unix seconds, with an optional word that says what it is
/// ("pushed", "updated").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptFilterTime {
    pub at: i64,
    pub label: Option<String>,
}

impl ScriptFilterTime {
    /// How long ago, as shown after the subtitle: `pushed 3h ago`.
    pub fn text(&self, now: i64) -> String {
        let relative = relative(now, self.at);
        match &self.label {
            Some(label) => format!("{label} {relative}"),
            None => relative,
        }
    }

    /// The preview row for this time: `Pushed: 3h ago · 2026-11-19 05:10`.
    pub fn metadata_row(&self, now: i64, zone: &jiff::tz::TimeZone) -> (String, String) {
        let mut name = self.label.clone().unwrap_or_else(|| "time".into());
        if let Some(first) = name.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let relative = relative(now, self.at);
        let value = match self.absolute(zone) {
            Some(absolute) => format!("{relative} \u{b7} {absolute}"),
            None => relative,
        };
        (name, value)
    }

    /// The moment in `zone`, such as `2026-11-19 05:10`.
    pub fn absolute(&self, zone: &jiff::tz::TimeZone) -> Option<String> {
        let moment = jiff::Timestamp::from_second(self.at).ok()?;
        Some(
            moment
                .to_zoned(zone.clone())
                .strftime("%Y-%m-%d %H:%M")
                .to_string(),
        )
    }
}

/// `3h ago`, `in 2d`, or `just now` within a minute either way. Whole units,
/// rounded down, the way `ghcommon.ago` does it.
pub(crate) fn relative(now: i64, at: i64) -> String {
    let elapsed = now.saturating_sub(at);
    let distance = elapsed.saturating_abs();
    if distance < MINUTE {
        return "just now".into();
    }
    let (count, unit) = match distance {
        d if d >= YEAR => (d / YEAR, "y"),
        d if d >= MONTH => (d / MONTH, "mo"),
        d if d >= DAY => (d / DAY, "d"),
        d if d >= HOUR => (d / HOUR, "h"),
        d => (d / MINUTE, "m"),
    };
    if elapsed >= 0 {
        format!("{count}{unit} ago")
    } else {
        format!("in {count}{unit}")
    }
}

impl super::types::ScriptFilterItem {
    /// The subtitle as drawn: what the script gave, then how long ago the
    /// item's `time` was, after a separator. Either alone when the other is
    /// missing.
    pub fn subtitle_with_time(&self, now: i64) -> Option<String> {
        match (&self.subtitle, &self.time) {
            (Some(subtitle), Some(time)) => {
                // The reset keeps a colour the subtitle left open off the time.
                Some(format!("{subtitle}\x1b[0m \u{b7} {}", time.text(now)))
            }
            (Some(subtitle), None) => Some(subtitle.clone()),
            (None, Some(time)) => Some(time.text(now)),
            (None, None) => None,
        }
    }
}

/// Seconds since the epoch.
pub(crate) fn now_unix() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn clean_label(label: &str) -> Option<String> {
    let label: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

fn parse_at(value: &serde_json::Value) -> Option<i64> {
    let seconds = match value {
        serde_json::Value::Number(number) => number.as_i64()?,
        serde_json::Value::String(text) => text.parse::<jiff::Timestamp>().ok()?.as_second(),
        _ => return None,
    };
    // Anything too far from now cannot be a real moment.
    (seconds.saturating_sub(now_unix()).saturating_abs() <= MAX_DISTANCE).then_some(seconds)
}

/// A `time` value: RFC 3339 text, integer seconds, or `{"at": ..., "label":
/// "..."}`. Anything else is no time.
pub(crate) fn parse_time(raw: &str) -> Option<ScriptFilterTime> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    match &value {
        serde_json::Value::Object(fields) => Some(ScriptFilterTime {
            at: parse_at(fields.get("at")?)?,
            label: fields
                .get("label")
                .and_then(serde_json::Value::as_str)
                .and_then(clean_label),
        }),
        other => Some(ScriptFilterTime {
            at: parse_at(other)?,
            label: None,
        }),
    }
}

/// For `#[serde(deserialize_with)]`: a time that does not fit leaves the item
/// without one, and never fails the list. The raw text is kept because an
/// out-of-range number would not survive being read as a value.
pub(crate) fn lenient_time<'de, D>(deserializer: D) -> Result<Option<ScriptFilterTime>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<Box<RawValue>>::deserialize(deserializer)?;
    Ok(raw.and_then(|raw| parse_time(raw.get())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn relative_time_uses_whole_units_rounded_down() {
        let ago = |seconds: i64| relative(NOW, NOW - seconds);
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(60), "1m ago");
        assert_eq!(ago(3_599), "59m ago");
        assert_eq!(ago(3_600), "1h ago");
        assert_eq!(ago(86_399), "23h ago");
        assert_eq!(ago(86_400), "1d ago");
        assert_eq!(ago(30 * 86_400 - 1), "29d ago");
        assert_eq!(ago(30 * 86_400), "1mo ago");
        assert_eq!(ago(365 * 86_400 - 1), "12mo ago");
        assert_eq!(ago(365 * 86_400), "1y ago");
        assert_eq!(ago(3 * 365 * 86_400), "3y ago");
    }

    #[test]
    fn a_time_in_the_future_reads_in() {
        let ahead = |seconds: i64| relative(NOW, NOW + seconds);
        assert_eq!(ahead(30), "just now", "clock skew is absorbed");
        assert_eq!(ahead(60), "in 1m");
        assert_eq!(ahead(2 * 86_400), "in 2d");
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        assert_eq!(relative(i64::MIN, i64::MAX), "in 292471208677y");
        assert!(relative(i64::MAX, i64::MIN).ends_with("y ago"));
        assert_eq!(relative(0, 0), "just now");
    }

    fn parse(raw: &str) -> Option<ScriptFilterTime> {
        parse_time(raw)
    }

    fn near(seconds: i64) -> String {
        (now_unix() - seconds).to_string()
    }

    #[test]
    fn a_time_is_read_as_rfc3339_text_integer_seconds_or_an_object() {
        let moment = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(3);
        let text = parse(&format!("\"{moment}\"")).unwrap();
        assert_eq!(text.at, moment.as_second());
        assert_eq!(text.label, None);
        let number = parse(&near(3_600)).unwrap();
        assert!((now_unix() - number.at - 3_600).abs() <= 1);
        let object = parse(&format!(r#"{{"at":{},"label":"pushed"}}"#, near(7_200))).unwrap();
        assert_eq!(object.label.as_deref(), Some("pushed"));
        let offset = parse(r#""2026-11-19T07:10:04+02:00""#).unwrap();
        assert_eq!(offset.at, parse(r#""2026-11-19T05:10:04Z""#).unwrap().at);
    }

    #[test]
    fn anything_else_is_no_time() {
        for bad in [
            "null",
            "true",
            "[]",
            "{}",
            r#"{"label":"pushed"}"#,
            r#"{"at":"soon"}"#,
            r#""yesterday""#,
            r#""""#,
            "1.5",
            "-1e400",
            "1e400",
            "99999999999999999999",
            "9223372036854775807",
            "-9223372036854775808",
            "-100000000000",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_label_is_cleaned_and_capped() {
        let raw = format!(
            r#"{{"at":{},"label":"\u001b[31m  pushed\nlast  {}"}}"#,
            near(10),
            "x".repeat(60)
        );
        let label = parse(&raw).unwrap().label.unwrap();
        assert!(!label.contains('\u{1b}') && !label.contains('\n'));
        assert_eq!(label.chars().count(), 32);
        assert!(label.starts_with("[31m  pushedlast"));
        let empty = parse(&format!(r#"{{"at":{},"label":"  \n"}}"#, near(10))).unwrap();
        assert_eq!(empty.label, None);
    }

    #[test]
    fn the_text_puts_the_label_first() {
        let time = ScriptFilterTime {
            at: NOW - 3 * 3_600,
            label: Some("pushed".into()),
        };
        assert_eq!(time.text(NOW), "pushed 3h ago");
        let bare = ScriptFilterTime {
            at: NOW - 3 * 3_600,
            label: None,
        };
        assert_eq!(bare.text(NOW), "3h ago");
    }

    #[test]
    fn the_preview_row_names_the_time_and_gives_both_forms() {
        let utc = jiff::tz::TimeZone::UTC;
        let time = ScriptFilterTime {
            at: 1_795_065_004,
            label: Some("pushed".into()),
        };
        assert_eq!(
            time.metadata_row(1_795_065_004 + 3 * 3_600, &utc),
            (
                "Pushed".to_string(),
                "3h ago \u{b7} 2026-11-19 05:10".to_string()
            )
        );
        let bare = ScriptFilterTime {
            at: 1_795_065_004,
            label: None,
        };
        assert_eq!(bare.metadata_row(1_795_065_004, &utc).0, "Time");
    }

    #[test]
    fn the_absolute_time_follows_the_zone_it_is_given() {
        let time = ScriptFilterTime {
            at: 1_795_065_004,
            label: None,
        };
        let utc = jiff::tz::TimeZone::UTC;
        assert_eq!(time.absolute(&utc).as_deref(), Some("2026-11-19 05:10"));
        let paris = jiff::tz::TimeZone::fixed(jiff::tz::offset(2));
        assert_eq!(time.absolute(&paris).as_deref(), Some("2026-11-19 07:10"));
    }
}
