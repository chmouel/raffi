use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use jiff::civil::Time;
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{Timestamp, Zoned};

use super::preview::{PreviewBody, PreviewData};
use super::types::{InlineProvider, InlineResult, InlineRow};

const MISSING_DATA: &str = "Time zone data not found (install tzdata)";
const MAX_ROWS: usize = 8;

/// Short names that are not the city part of an IANA zone name.
const ALIASES: &[(&str, &str)] = &[
    ("nyc", "America/New_York"),
    ("ny", "America/New_York"),
    ("boston", "America/New_York"),
    ("washington", "America/New_York"),
    ("dc", "America/New_York"),
    ("sf", "America/Los_Angeles"),
    ("san francisco", "America/Los_Angeles"),
    ("la", "America/Los_Angeles"),
    ("seattle", "America/Los_Angeles"),
    ("uk", "Europe/London"),
    ("utc", "Etc/UTC"),
    ("gmt", "Etc/GMT"),
    ("beijing", "Asia/Shanghai"),
    ("mumbai", "Asia/Kolkata"),
    ("delhi", "Asia/Kolkata"),
    ("new delhi", "Asia/Kolkata"),
    ("bangalore", "Asia/Kolkata"),
];

type ZoneLoader = Box<dyn Fn(&str) -> Option<TimeZone> + Send + Sync>;

/// Place names mapped to IANA zone names, plus the loader for those zones.
pub(super) struct ZoneIndex {
    names: HashSet<String>,
    places: HashMap<String, Vec<String>>,
    load: ZoneLoader,
}

impl ZoneIndex {
    pub(super) fn new(
        names: impl IntoIterator<Item = String>,
        load: impl Fn(&str) -> Option<TimeZone> + Send + Sync + 'static,
    ) -> Self {
        let names: HashSet<String> = names
            .into_iter()
            .filter(|name| {
                !name.starts_with("posix/")
                    && !name.starts_with("right/")
                    && name != "posixrules"
                    && name != "Factory"
            })
            .collect();
        let mut places: HashMap<String, Vec<String>> = HashMap::new();
        for name in &names {
            let city = name.rsplit('/').next().unwrap_or(name);
            let mut keys = vec![normalize(name)];
            match utc_offset_keys(name) {
                Some(offset_keys) => keys.extend(offset_keys),
                None => keys.push(normalize(city)),
            }
            for key in keys {
                let zones = places.entry(key).or_default();
                if !zones.contains(name) {
                    zones.push(name.clone());
                }
            }
        }
        for zones in places.values_mut() {
            zones.sort();
        }
        Self {
            names,
            places,
            load: Box::new(load),
        }
    }

    #[cfg(not(test))]
    fn system() -> Self {
        let db = jiff::tz::db();
        Self::new(
            db.available().map(|name| name.as_str().to_string()),
            |name| jiff::tz::db().get(name).ok(),
        )
    }

    /// Zones matching `place`; a bare legacy name like `UTC` gives way to a
    /// `Region/City` name matching the same place.
    fn resolve(&self, place: &str) -> Option<Vec<String>> {
        let place = normalize(place);
        if let Some((_, zone)) = ALIASES.iter().find(|(alias, _)| *alias == place) {
            if self.names.contains(*zone) {
                return Some(vec![zone.to_string()]);
            }
        }
        let zones = self.places.get(&place)?;
        let regional: Vec<String> = zones.iter().filter(|z| z.contains('/')).cloned().collect();
        if regional.is_empty() {
            Some(zones.clone())
        } else {
            Some(regional)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TimeQuery {
    /// The current time in each zone.
    Now(Vec<String>),
    /// `time` today in each `from` zone, shown in each `to` zone.
    Convert {
        time: Time,
        from: Vec<String>,
        to: Vec<String>,
    },
    MissingData,
}

/// The query's shape before any place name is looked up.
enum Shape<'a> {
    Now(String),
    Convert(Time, Vec<&'a str>),
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `gmt±N` and `utc±N` for the zone `Etc/GMT∓N`: the zone database
/// flips the sign, so `Etc/GMT+2` is two hours behind UTC.
fn utc_offset_keys(name: &str) -> Option<[String; 2]> {
    let offset = name.strip_prefix("Etc/GMT")?;
    let (sign, hours) = if let Some(hours) = offset.strip_prefix('+') {
        ('-', hours)
    } else {
        ('+', offset.strip_prefix('-')?)
    };
    if hours.is_empty() || !hours.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some([format!("gmt{sign}{hours}"), format!("utc{sign}{hours}")])
}

fn shape(query: &str) -> Option<Shape<'_>> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let (first, rest) = words.split_first()?;
    let first = first.to_lowercase();
    if first == "time"
        || (first == "now" && rest.first().map(|w| w.to_lowercase()) == Some("in".into()))
    {
        let place = match rest.first() {
            Some(word) if word.eq_ignore_ascii_case("in") => &rest[1..],
            _ => rest,
        };
        return (!place.is_empty()).then(|| Shape::Now(place.join(" ")));
    }
    let time = parse_clock(&first)?;
    (rest.len() >= 3).then(|| Shape::Convert(time, rest.to_vec()))
}

/// `15:00`, `9:30`, `3pm` or `3:30pm`.
fn parse_clock(token: &str) -> Option<Time> {
    let (clock, meridiem) = if let Some(clock) = token.strip_suffix("am") {
        (clock, Some(0))
    } else if let Some(clock) = token.strip_suffix("pm") {
        (clock, Some(12))
    } else {
        (token, None)
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) if minute.len() == 2 => (hour, minute),
        Some(_) => return None,
        None if meridiem.is_some() => (clock, "00"),
        None => return None,
    };
    if hour.is_empty()
        || hour.len() > 2
        || !hour
            .bytes()
            .chain(minute.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut hour: i8 = hour.parse().ok()?;
    let minute: i8 = minute.parse().ok()?;
    if let Some(offset) = meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + offset;
    }
    Time::new(hour, minute, 0, 0).ok()
}

pub(super) fn parse_time_query(query: &str, index: &ZoneIndex) -> Option<TimeQuery> {
    match shape(query)? {
        Shape::Now(_) if index.names.is_empty() => Some(TimeQuery::MissingData),
        Shape::Now(place) => index.resolve(&place).map(TimeQuery::Now),
        Shape::Convert(time, words) => {
            for split in 1..words.len() - 1 {
                if !["to", "in"].contains(&words[split].to_lowercase().as_str()) {
                    continue;
                }
                if index.names.is_empty() {
                    return Some(TimeQuery::MissingData);
                }
                let from = index.resolve(&words[..split].join(" "));
                let to = index.resolve(&words[split + 1..].join(" "));
                if let (Some(from), Some(to)) = (from, to) {
                    return Some(TimeQuery::Convert { time, from, to });
                }
            }
            None
        }
    }
}

fn system_zones() -> &'static ZoneIndex {
    static ZONES: OnceLock<ZoneIndex> = OnceLock::new();
    #[cfg(test)]
    let init = fixtures::index;
    #[cfg(not(test))]
    let init = ZoneIndex::system;
    ZONES.get_or_init(init)
}

fn local_zone() -> TimeZone {
    if cfg!(test) {
        TimeZone::UTC
    } else {
        TimeZone::system()
    }
}

/// Parses `query` against the system time zone database; ordinary queries
/// are rejected before the database is read.
pub(super) fn try_parse_time_query(query: &str) -> Option<TimeQuery> {
    shape(query)?;
    parse_time_query(query, system_zones())
}

pub(super) fn current_time_inline_result(query: &TimeQuery) -> InlineResult {
    time_inline_result(query, system_zones(), Timestamp::now(), &local_zone())
}

fn status_result() -> InlineResult {
    InlineResult {
        provider: InlineProvider::Time,
        rows: vec![InlineRow {
            title: MISSING_DATA.to_string(),
            subtitle: None,
            copy_value: String::new(),
            alt_copy_value: None,
            swatch: None,
            preview: None,
        }],
    }
}

fn day_label(days: i32) -> String {
    match days {
        1 => "+1 day".to_string(),
        -1 => "-1 day".to_string(),
        _ => format!("{days:+} days"),
    }
}

/// One row for `zoned`, dated when it falls on another day than
/// `reference`; `source` describes the converted time, if any.
fn zone_row(
    zoned: &Zoned,
    name: &str,
    reference: jiff::civil::Date,
    source: Option<&str>,
) -> InlineRow {
    let days = reference
        .until(zoned.date())
        .map(|span| span.get_days())
        .unwrap_or(0);
    let time = zoned.strftime("%H:%M").to_string();
    let title = if days == 0 {
        time
    } else {
        format!("{time}, {}", zoned.strftime("%a %-d %b"))
    };
    let offset = format!("{} UTC{}", zoned.strftime("%Z"), zoned.strftime("%:z"));
    let iso = zoned.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string();
    let zone = match source {
        Some(source) => format!("{source} → {name}"),
        None => name.to_string(),
    };
    let mut subtitle = format!("{zone} · {offset}");
    if days != 0 {
        subtitle = format!("{} · {subtitle}", day_label(days));
    }
    let preview = PreviewData {
        title: "World clock".to_string(),
        subtitle: Some(zone),
        badge: Some(offset.clone()),
        icon: None,
        bodies: vec![PreviewBody::HeroValue {
            expression: None,
            value: title.clone(),
        }],
        metadata: vec![
            ("Time zone".to_string(), name.to_string()),
            ("Offset".to_string(), offset),
            ("Enter".to_string(), format!("Copy {title} to clipboard")),
            ("Ctrl+Enter".to_string(), format!("Copy {iso} to clipboard")),
        ],
    };
    InlineRow {
        copy_value: title.clone(),
        title,
        subtitle: Some(subtitle),
        alt_copy_value: Some(iso),
        swatch: None,
        preview: Some(preview),
    }
}

/// Rows for `query` at instant `now`; current-time rows are dated relative
/// to `local`. Any zone that fails to load turns the result into a status row.
pub(super) fn time_inline_result(
    query: &TimeQuery,
    index: &ZoneIndex,
    now: Timestamp,
    local: &TimeZone,
) -> InlineResult {
    let rows = match query {
        TimeQuery::MissingData => None,
        TimeQuery::Now(zones) => {
            let today = now.to_zoned(local.clone()).date();
            zones
                .iter()
                .take(MAX_ROWS)
                .map(|name| {
                    let tz = (index.load)(name)?;
                    Some(zone_row(&now.to_zoned(tz), name, today, None))
                })
                .collect::<Option<Vec<_>>>()
        }
        TimeQuery::Convert { time, from, to } => from
            .iter()
            .flat_map(|source| to.iter().map(move |target| (source, target)))
            .take(MAX_ROWS)
            .map(|(source, target)| {
                let source_tz = (index.load)(source)?;
                let target_tz = (index.load)(target)?;
                let date = now.to_zoned(source_tz.clone()).date();
                let ambiguous = source_tz.to_ambiguous_zoned(date.to_datetime(*time));
                let gap = matches!(ambiguous.offset(), AmbiguousOffset::Gap { .. });
                let start = ambiguous.compatible().ok()?;
                let mut from = format!("{} {source}", start.strftime("%H:%M"));
                if gap {
                    from = format!(
                        "{} skipped by daylight saving, using {from}",
                        time.strftime("%H:%M")
                    );
                }
                let end = start.with_time_zone(target_tz);
                Some(zone_row(&end, target, start.date(), Some(&from)))
            })
            .collect::<Option<Vec<_>>>(),
    };
    match rows {
        Some(rows) if !rows.is_empty() => InlineResult {
            provider: InlineProvider::Time,
            rows,
        },
        _ => status_result(),
    }
}

#[cfg(test)]
mod fixtures {
    use super::{TimeZone, ZoneIndex};

    const ZONES: &[(&str, &[u8])] = &[
        (
            "Europe/Paris",
            include_bytes!("../../../tests/fixtures/tz/Europe/Paris"),
        ),
        (
            "Asia/Kolkata",
            include_bytes!("../../../tests/fixtures/tz/Asia/Kolkata"),
        ),
        (
            "Pacific/Kiritimati",
            include_bytes!("../../../tests/fixtures/tz/Pacific/Kiritimati"),
        ),
        (
            "Etc/GMT+12",
            include_bytes!("../../../tests/fixtures/tz/Etc/GMT+12"),
        ),
        (
            "Etc/GMT-2",
            include_bytes!("../../../tests/fixtures/tz/Etc/GMT-2"),
        ),
        (
            "America/New_York",
            include_bytes!("../../../tests/fixtures/tz/America/New_York"),
        ),
        (
            "US/Eastern",
            include_bytes!("../../../tests/fixtures/tz/America/New_York"),
        ),
        (
            "Canada/Eastern",
            include_bytes!("../../../tests/fixtures/tz/America/New_York"),
        ),
        (
            "Asia/Tokyo",
            include_bytes!("../../../tests/fixtures/tz/Asia/Tokyo"),
        ),
    ];

    pub(super) fn load(name: &str) -> Option<TimeZone> {
        if name == "UTC" || name == "Etc/UTC" {
            return Some(TimeZone::UTC);
        }
        let (_, data) = ZONES.iter().find(|(zone, _)| *zone == name)?;
        TimeZone::tzif(name, data).ok()
    }

    pub(super) fn index() -> ZoneIndex {
        let names = ZONES
            .iter()
            .map(|(name, _)| *name)
            .chain(["UTC", "Etc/UTC"])
            .map(str::to_string);
        ZoneIndex::new(names, load)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(instant: &str) -> Timestamp {
        instant.parse().expect("valid instant")
    }

    fn rows(query: &str, now: &str) -> Vec<(String, String)> {
        let index = fixtures::index();
        let parsed = parse_time_query(query, &index).unwrap_or_else(|| panic!("{query}"));
        time_inline_result(&parsed, &index, at(now), &TimeZone::UTC)
            .rows
            .into_iter()
            .map(|row| (row.title, row.subtitle.unwrap_or_default()))
            .collect()
    }

    fn titles(query: &str, now: &str) -> Vec<String> {
        rows(query, now)
            .into_iter()
            .map(|(title, _)| title)
            .collect()
    }

    #[test]
    fn test_current_time_in_named_and_aliased_places() {
        let now = "2026-10-16T12:00:00Z";
        assert_eq!(titles("time tokyo", now), ["21:00"]);
        assert_eq!(titles("Time in New York", now), ["08:00"]);
        assert_eq!(titles("time nyc", now), ["08:00"]);
        assert_eq!(titles("now in america/new_york", now), ["08:00"]);
        assert_eq!(titles("time utc", now), ["12:00"]);
        assert_eq!(titles("time kolkata", now), ["17:30"]);
        let (_, subtitle) = &rows("time kolkata", now)[0];
        assert_eq!(subtitle, "Asia/Kolkata · IST UTC+05:30");
    }

    #[test]
    fn test_current_time_row_dates_another_day_relative_to_local() {
        let (title, subtitle) = &rows("time tokyo", "2026-10-16T20:00:00Z")[0];
        assert_eq!(title, "05:00, Sat 17 Oct");
        assert!(subtitle.starts_with("+1 day · Asia/Tokyo"), "{subtitle}");
    }

    #[test]
    fn test_conversion_uses_today_in_the_source_zone() {
        let now = "2026-10-16T12:00:00Z";
        assert_eq!(titles("15:00 paris to nyc", now), ["09:00"]);
        assert_eq!(titles("3pm paris in kolkata", now), ["18:30"]);
        let (title, subtitle) = &rows("9am tokyo to new york", now)[0];
        assert_eq!(title, "20:00, Thu 15 Oct");
        assert_eq!(
            subtitle,
            "-1 day · 09:00 Asia/Tokyo → America/New_York · EDT UTC-04:00"
        );
    }

    #[test]
    fn test_dst_gap_moves_forward_and_fold_takes_the_earlier_instant() {
        let (title, subtitle) = &rows("2:30 paris to tokyo", "2026-03-29T00:00:00Z")[0];
        assert_eq!(title, "10:30");
        assert!(
            subtitle.starts_with("02:30 skipped by daylight saving, using 03:30 Europe/Paris"),
            "{subtitle}"
        );
        assert_eq!(
            titles("2:30 paris to tokyo", "2026-10-25T00:00:00Z"),
            ["09:30"]
        );
    }

    #[test]
    fn test_day_difference_reaches_two_days_each_way() {
        let (title, subtitle) = &rows("11:30pm gmt-12 to kiritimati", "2026-10-16T12:00:00Z")[0];
        assert_eq!(title, "01:30, Sun 18 Oct");
        assert!(subtitle.starts_with("+2 days"), "{subtitle}");
        let (title, subtitle) = &rows("1am kiritimati to gmt-12", "2026-10-16T12:00:00Z")[0];
        assert_eq!(title, "23:00, Thu 15 Oct");
        assert!(subtitle.starts_with("-2 days"), "{subtitle}");
    }

    #[test]
    fn test_gmt_and_utc_offsets_are_ahead_of_utc_when_positive() {
        let now = "2026-10-16T12:00:00Z";
        for query in ["time gmt+2", "time utc+2", "time GMT+2"] {
            let (title, subtitle) = &rows(query, now)[0];
            assert_eq!(title, "14:00", "{query}");
            assert!(subtitle.ends_with("UTC+02:00"), "{query}: {subtitle}");
        }
        let (title, subtitle) = &rows("time utc-12", now)[0];
        assert_eq!(title, "00:00");
        assert!(subtitle.ends_with("UTC-12:00"), "{subtitle}");
        // The full zone name keeps the zone database's inverted sign.
        let (title, subtitle) = &rows("time etc/gmt+12", now)[0];
        assert_eq!(title, "00:00");
        assert!(subtitle.ends_with("UTC-12:00"), "{subtitle}");
        assert_eq!(
            parse_time_query("time gmt+12", &fixtures::index()),
            None,
            "no zone is twelve hours ahead in the fixtures"
        );
    }

    #[test]
    fn test_ambiguous_place_gives_one_row_per_zone() {
        let rows = rows("time eastern", "2026-10-16T12:00:00Z");
        let zones: Vec<&str> = rows
            .iter()
            .map(|(_, subtitle)| subtitle.split(" · ").next().unwrap())
            .collect();
        assert_eq!(zones, ["Canada/Eastern", "US/Eastern"]);
    }

    #[test]
    fn test_copy_values_are_display_time_and_rfc3339() {
        let index = fixtures::index();
        let parsed = parse_time_query("time tokyo", &index).unwrap();
        let result =
            time_inline_result(&parsed, &index, at("2026-10-16T20:00:00Z"), &TimeZone::UTC);
        let row = &result.rows[0];
        assert_eq!(row.copy_value, row.title);
        assert_eq!(
            row.alt_copy_value.as_deref(),
            Some("2026-10-17T05:00:00+09:00")
        );
    }

    #[test]
    fn test_rejects_unrelated_or_invalid_queries() {
        let index = fixtures::index();
        for query in [
            "time",
            "timeshift",
            "time foo",
            "time in",
            "now",
            "now paris",
            "firefox",
            "15:00",
            "15:00 paris",
            "15:00 paris to",
            "15:00 paris to atlantis",
            "25:00 paris to tokyo",
            "13pm paris to tokyo",
            "0am paris to tokyo",
            "15:5 paris to tokyo",
            "10 km to mi",
        ] {
            assert_eq!(parse_time_query(query, &index), None, "{query}");
        }
    }

    #[test]
    fn test_missing_or_unloadable_zone_data_gives_a_status_row() {
        let empty = ZoneIndex::new(Vec::new(), |_| None);
        assert_eq!(
            parse_time_query("time tokyo", &empty),
            Some(TimeQuery::MissingData)
        );
        assert_eq!(
            parse_time_query("15:00 paris to nyc", &empty),
            Some(TimeQuery::MissingData)
        );
        assert_eq!(parse_time_query("timeshift", &empty), None);

        let broken = ZoneIndex::new(vec!["Asia/Tokyo".to_string()], |_| None);
        let parsed = parse_time_query("time tokyo", &broken).unwrap();
        let result =
            time_inline_result(&parsed, &broken, at("2026-10-16T12:00:00Z"), &TimeZone::UTC);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].title, MISSING_DATA);
        assert!(result.rows[0].copy_value.is_empty());
    }
}
