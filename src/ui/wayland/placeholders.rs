//! Alfred-style dynamic placeholders for text snippets.
//!
//! Parsing and formatting are pure; anything with side effects (clock,
//! clipboard, environment, shell, randomness) goes through [`Providers`] so the
//! expander can be tested deterministically.

use std::collections::BTreeSet;
use std::fmt::{self, Write as _};

use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Days, LocalResult, Months, TimeDelta, TimeZone};

use crate::TextSnippet;

/// Largest value a snippet may expand to. Kept under Linux's 128 KiB limit for
/// a single argument or environment string, which is how values reach the
/// copy, insert and custom action commands.
pub(super) const MAX_VALUE: usize = 100 * 1024;

const MAX_SNIPPET_DEPTH: usize = 5;
/// Total `{snippet:...}` expansions allowed for one template, so snippets that
/// reference several others cannot fan out exponentially within the depth
/// limit.
const MAX_SNIPPET_EXPANSIONS: usize = 1000;
const MAX_RANDOM_ATTEMPTS: usize = 64;
/// Highest `N` accepted by `{clipboard:N}`.
pub(super) const MAX_HISTORY_INDEX: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Cheap expansion for the subtitle: never runs shell commands or draws
    /// random values, and only uses an already fetched clipboard.
    Preview,
    /// Full expansion right before the action runs.
    Final,
}

pub(super) trait Providers {
    type Tz: TimeZone;

    fn now(&mut self) -> DateTime<Self::Tz>;
    fn clipboard(&mut self) -> Result<String, String>;
    /// Entry `index` of the clipboard manager history, newest first. Never
    /// called with 0, which is the current clipboard.
    fn clipboard_history(&mut self, index: usize) -> Result<String, String>;
    fn env(&mut self, name: &str) -> Option<String>;
    fn shell(&mut self, command: &str) -> Result<String, String>;
    fn random_u64(&mut self) -> Result<u64, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Expanded {
    pub text: String,
    /// Characters between the `{cursor}` marker and the end of the text.
    pub cursor_moves: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Name {
    Clipboard,
    Date,
    Time,
    DateTime,
    Random,
    Env,
    Snippet,
    Shell,
    Cursor,
}

impl Name {
    fn parse(ident: &str) -> Option<Self> {
        Some(match ident {
            "clipboard" => Self::Clipboard,
            "date" => Self::Date,
            "time" => Self::Time,
            "datetime" => Self::DateTime,
            "random" => Self::Random,
            "env" => Self::Env,
            "snippet" => Self::Snippet,
            "shell" => Self::Shell,
            "cursor" => Self::Cursor,
            _ => return None,
        })
    }

    fn is_date(self) -> bool {
        matches!(self, Self::Date | Self::Time | Self::DateTime)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Offset {
    negative: bool,
    amount: u32,
    unit: Unit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transform {
    Uppercase,
    Lowercase,
    Capitals,
    Trim,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Spec {
    offsets: Vec<Offset>,
    arg: Option<String>,
    transforms: Vec<Transform>,
}

#[derive(Debug)]
enum Piece {
    Text(String),
    Token {
        raw: String,
        name: Name,
        spec: Result<Spec, String>,
    },
}

/// Recognise a placeholder at the start of `s` (which begins with `{`).
/// Returns the placeholder name, its body after the name, and the number of
/// bytes consumed. Anything unrecognised returns `None` and stays literal.
fn scan_token(s: &str, allow_shell: bool) -> Option<(Name, String, usize)> {
    let rest = &s[1..];
    let ident_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_lowercase())
        .count();
    let name = Name::parse(&rest[..ident_len])?;
    let after = &rest[ident_len..];
    if !matches!(after.as_bytes().first(), Some(b'}' | b':' | b' ' | b'|')) {
        return None;
    }

    if name == Name::Shell {
        if !allow_shell {
            return None;
        }
        let mut body = String::new();
        let mut chars = after.char_indices();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '\\' if after[index + 1..].starts_with('}') => {
                    body.push('}');
                    chars.next();
                }
                '}' => return Some((name, body, 1 + ident_len + index + 1)),
                _ => body.push(ch),
            }
        }
        return None;
    }

    let close = after.find('}')?;
    Some((name, after[..close].to_string(), 1 + ident_len + close + 1))
}

fn parse(template: &str, allow_shell: bool) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut index = 0;

    while index < template.len() {
        let rest = &template[index..];
        if rest.starts_with("\\{") {
            // Escapes recognise every name, so `\{shell:x}` is literal
            // `{shell:x}` even where shell placeholders are disabled.
            if let Some((_, _, len)) = scan_token(&rest[1..], true) {
                text.push_str(&rest[1..1 + len]);
                index += 1 + len;
                continue;
            }
        }
        if rest.starts_with('{') {
            if let Some((name, body, len)) = scan_token(rest, allow_shell) {
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(Piece::Token {
                    raw: rest[..len].to_string(),
                    name,
                    spec: parse_spec(name, &body),
                });
                index += len;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or_default();
        text.push(ch);
        index += ch.len_utf8();
    }

    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    pieces
}

fn parse_offset(token: &str) -> Result<Offset, String> {
    let invalid = || format!("invalid date offset {token:?} (expected e.g. +1d or -2h)");
    let (negative, rest) = match token.as_bytes().first() {
        Some(b'+') => (false, &token[1..]),
        Some(b'-') => (true, &token[1..]),
        _ => return Err(invalid()),
    };
    let unit_char = rest.chars().last().ok_or_else(invalid)?;
    let unit = match unit_char {
        's' => Unit::Seconds,
        'm' => Unit::Minutes,
        'h' => Unit::Hours,
        'd' => Unit::Days,
        'w' => Unit::Weeks,
        'M' => Unit::Months,
        'y' => Unit::Years,
        _ => return Err(invalid()),
    };
    let amount = rest[..rest.len() - 1]
        .parse::<u32>()
        .map_err(|_| invalid())?;
    Ok(Offset {
        negative,
        amount,
        unit,
    })
}

fn parse_transform(name: &str) -> Result<Transform, String> {
    match name.trim() {
        "uppercase" => Ok(Transform::Uppercase),
        "lowercase" => Ok(Transform::Lowercase),
        "capitals" => Ok(Transform::Capitals),
        "trim" => Ok(Transform::Trim),
        other => Err(format!(
            "unknown transform {other:?} (use uppercase, lowercase, capitals or trim)"
        )),
    }
}

fn validate_strftime(format: &str) -> Result<(), String> {
    if StrftimeItems::new(format).any(|item| matches!(item, Item::Error)) {
        Err(format!("invalid date format {format:?}"))
    } else {
        Ok(())
    }
}

fn parse_random_range(arg: &str) -> Result<(u64, u64), String> {
    let invalid = || format!("invalid random {arg:?} (use uuid or a range like 1-100)");
    let (low, high) = arg.split_once('-').ok_or_else(invalid)?;
    let low = low.trim().parse::<u64>().map_err(|_| invalid())?;
    let high = high.trim().parse::<u64>().map_err(|_| invalid())?;
    if low > high {
        return Err(format!("invalid random range {arg:?}: start is above end"));
    }
    Ok((low, high))
}

fn parse_spec(name: Name, body: &str) -> Result<Spec, String> {
    if name == Name::Shell {
        let command = body
            .strip_prefix(':')
            .filter(|command| !command.trim().is_empty())
            .ok_or("{shell:...} needs a command")?;
        return Ok(Spec {
            arg: Some(command.to_string()),
            ..Spec::default()
        });
    }

    let (head, transforms) = match body.split_once('|') {
        Some((head, transforms)) => (
            head,
            transforms
                .split('|')
                .map(parse_transform)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => (body, Vec::new()),
    };
    let (offsets_part, arg) = match head.split_once(':') {
        Some((offsets, arg)) => (offsets, Some(arg.to_string())),
        None => (head, None),
    };
    let offsets = offsets_part
        .split_whitespace()
        .map(parse_offset)
        .collect::<Result<Vec<_>, _>>()?;

    if !offsets.is_empty() && !name.is_date() {
        return Err("date offsets only work with {date}, {time} and {datetime}".into());
    }

    match name {
        Name::Clipboard => {
            clipboard_index(arg.as_deref())?;
        }
        Name::Cursor if arg.is_some() || !transforms.is_empty() => {
            return Err("{cursor} takes no argument or transform".into())
        }
        Name::Env | Name::Snippet if arg.as_deref().is_none_or(|arg| arg.trim().is_empty()) => {
            let label = if name == Name::Env { "env" } else { "snippet" };
            return Err(format!("{{{label}:...}} needs a name"));
        }
        Name::Random => match arg.as_deref() {
            Some("uuid") => {}
            Some(range) => {
                parse_random_range(range)?;
            }
            None => return Err("{random:...} needs uuid or a range like 1-100".into()),
        },
        Name::Date | Name::Time | Name::DateTime => {
            if let Some(format) = &arg {
                validate_strftime(format)?;
            }
        }
        _ => {}
    }

    Ok(Spec {
        offsets,
        arg,
        transforms,
    })
}

/// History index of a `{clipboard:N}` argument; 0 is the current clipboard.
fn clipboard_index(arg: Option<&str>) -> Result<usize, String> {
    let Some(arg) = arg.map(str::trim) else {
        return Ok(0);
    };
    arg.parse::<usize>()
        .ok()
        .filter(|index| *index <= MAX_HISTORY_INDEX && arg.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| {
            format!("{{clipboard:{arg}}} needs a history number from 0 to {MAX_HISTORY_INDEX}")
        })
}

fn apply_transforms(mut value: String, transforms: &[Transform]) -> String {
    for transform in transforms {
        value = match transform {
            Transform::Uppercase => value.to_uppercase(),
            Transform::Lowercase => value.to_lowercase(),
            Transform::Trim => value.trim().to_string(),
            Transform::Capitals => {
                let mut out = String::with_capacity(value.len());
                let mut at_word_start = true;
                for ch in value.chars() {
                    if at_word_start && ch.is_alphabetic() {
                        out.extend(ch.to_uppercase());
                    } else {
                        out.push(ch);
                    }
                    at_word_start = ch.is_whitespace();
                }
                out
            }
        };
    }
    value
}

fn resolve_local<Tz: TimeZone>(
    tz: &Tz,
    naive: chrono::NaiveDateTime,
) -> Result<DateTime<Tz>, String> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(datetime) => Ok(datetime),
        LocalResult::Ambiguous(earliest, _) => Ok(earliest),
        LocalResult::None => Err(format!(
            "{naive} does not exist in the local timezone (daylight saving gap)"
        )),
    }
}

fn apply_offset<Tz: TimeZone>(
    datetime: DateTime<Tz>,
    offset: Offset,
) -> Result<DateTime<Tz>, String> {
    let overflow = || "date offset is out of range".to_string();
    let amount = i64::from(offset.amount);
    let signed = if offset.negative { -amount } else { amount };
    let elapsed = |seconds: i64| {
        TimeDelta::try_seconds(signed.checked_mul(seconds).ok_or_else(overflow)?)
            .and_then(|delta| datetime.clone().checked_add_signed(delta))
            .ok_or_else(overflow)
    };

    match offset.unit {
        Unit::Seconds => elapsed(1),
        Unit::Minutes => elapsed(60),
        Unit::Hours => elapsed(3600),
        Unit::Days | Unit::Weeks => {
            let days = if offset.unit == Unit::Weeks {
                u64::from(offset.amount) * 7
            } else {
                u64::from(offset.amount)
            };
            let naive = datetime.naive_local();
            let shifted = if offset.negative {
                naive.checked_sub_days(Days::new(days))
            } else {
                naive.checked_add_days(Days::new(days))
            }
            .ok_or_else(overflow)?;
            resolve_local(&datetime.timezone(), shifted)
        }
        Unit::Months | Unit::Years => {
            let months = if offset.unit == Unit::Years {
                offset.amount.checked_mul(12).ok_or_else(overflow)?
            } else {
                offset.amount
            };
            let naive = datetime.naive_local();
            let shifted = if offset.negative {
                naive.checked_sub_months(Months::new(months))
            } else {
                naive.checked_add_months(Months::new(months))
            }
            .ok_or_else(overflow)?;
            resolve_local(&datetime.timezone(), shifted)
        }
    }
}

fn format_date<P: Providers>(providers: &mut P, name: Name, spec: &Spec) -> Result<String, String>
where
    <P::Tz as TimeZone>::Offset: fmt::Display,
{
    let mut datetime = providers.now();
    for offset in &spec.offsets {
        datetime = apply_offset(datetime, *offset)?;
    }
    let format = spec.arg.as_deref().unwrap_or(match name {
        Name::Time => "%H:%M",
        Name::DateTime => "%Y-%m-%d %H:%M",
        _ => "%Y-%m-%d",
    });
    let mut out = String::new();
    write!(
        out,
        "{}",
        datetime.format_with_items(StrftimeItems::new(format))
    )
    .map_err(|_| format!("cannot format date with {format:?}"))?;
    Ok(out)
}

fn random_in_range<P: Providers>(providers: &mut P, low: u64, high: u64) -> Result<u64, String> {
    let span = high - low;
    if span == u64::MAX {
        return providers.random_u64();
    }
    let count = span + 1;
    // Reject the tail that would make some values more likely than others.
    let zone = (u64::MAX / count) * count;
    for _ in 0..MAX_RANDOM_ATTEMPTS {
        let value = providers.random_u64()?;
        if value < zone {
            return Ok(low + value % count);
        }
    }
    Err("could not draw a random number".into())
}

fn random_uuid<P: Providers>(providers: &mut P) -> Result<String, String> {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&providers.random_u64()?.to_be_bytes());
    bytes[8..].copy_from_slice(&providers.random_u64()?.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn find_snippet(snippets: &[TextSnippet], reference: &str) -> Option<usize> {
    let reference = reference.trim();
    snippets
        .iter()
        .position(|snippet| snippet.name == reference)
        .or_else(|| {
            snippets
                .iter()
                .position(|snippet| snippet.keyword.as_deref() == Some(reference))
        })
}

struct Expander<'a, P: Providers> {
    providers: &'a mut P,
    snippets: &'a [TextSnippet],
    allow_shell: bool,
    mode: Mode,
    stack: Vec<usize>,
    snippet_expansions: usize,
}

impl<P: Providers> Expander<'_, P>
where
    <P::Tz as TimeZone>::Offset: fmt::Display,
{
    fn push(out: &mut String, value: &str) -> Result<(), String> {
        if out.len() + value.len() > MAX_VALUE {
            return Err(format!(
                "expanded snippet is larger than {} KiB",
                MAX_VALUE / 1024
            ));
        }
        out.push_str(value);
        Ok(())
    }

    /// Expand `template`. Returns the text and, for the top-level template,
    /// the byte position of the first `{cursor}`.
    fn expand(&mut self, template: &str) -> Result<(String, Option<usize>), String> {
        let mut out = String::new();
        let mut cursor = None;

        for piece in parse(template, self.allow_shell) {
            let (raw, name, spec) = match piece {
                Piece::Text(text) => {
                    Self::push(&mut out, &text)?;
                    continue;
                }
                Piece::Token { raw, name, spec } => (raw, name, spec?),
            };

            let value = match name {
                Name::Cursor => {
                    if self.stack.is_empty() && cursor.is_none() {
                        cursor = Some(out.len());
                    }
                    continue;
                }
                Name::Shell | Name::Random if self.mode == Mode::Preview => {
                    Self::push(&mut out, &raw)?;
                    continue;
                }
                Name::Shell => {
                    let command = spec.arg.as_deref().unwrap_or_default();
                    self.providers
                        .shell(command)
                        .map_err(|error| format!("{{shell:{command}}} failed: {error}"))?
                }
                Name::Clipboard => match clipboard_index(spec.arg.as_deref())? {
                    0 => self
                        .providers
                        .clipboard()
                        .map_err(|error| format!("cannot read clipboard: {error}"))?,
                    index => self.providers.clipboard_history(index).map_err(|error| {
                        format!("cannot read clipboard history entry {index}: {error}")
                    })?,
                },
                Name::Date | Name::Time | Name::DateTime => {
                    format_date(self.providers, name, &spec)?
                }
                Name::Env => {
                    let variable = spec.arg.as_deref().unwrap_or_default().trim();
                    self.providers.env(variable).unwrap_or_default()
                }
                Name::Random => match spec.arg.as_deref() {
                    Some("uuid") => random_uuid(self.providers)?,
                    Some(range) => {
                        let (low, high) = parse_random_range(range)?;
                        random_in_range(self.providers, low, high)?.to_string()
                    }
                    None => unreachable!("validated in parse_spec"),
                },
                Name::Snippet => {
                    let reference = spec.arg.as_deref().unwrap_or_default();
                    let index = find_snippet(self.snippets, reference)
                        .ok_or_else(|| format!("no snippet named {:?}", reference.trim()))?;
                    if self.stack.contains(&index) {
                        return Err(format!(
                            "snippet {:?} includes itself",
                            self.snippets[index].name
                        ));
                    }
                    if self.stack.len() >= MAX_SNIPPET_DEPTH {
                        return Err("snippets are nested too deeply".into());
                    }
                    self.snippet_expansions += 1;
                    if self.snippet_expansions > MAX_SNIPPET_EXPANSIONS {
                        return Err(format!(
                            "snippet references expand more than {MAX_SNIPPET_EXPANSIONS} times"
                        ));
                    }
                    self.stack.push(index);
                    let template = self.snippets[index].value.clone();
                    let result = self.expand(&template);
                    self.stack.pop();
                    result?.0
                }
            };
            Self::push(&mut out, &apply_transforms(value, &spec.transforms))?;
        }

        Ok((out, cursor))
    }
}

/// Expand every placeholder in `template`.
///
/// `snippets` is the list `{snippet:...}` resolves against. When
/// `allow_shell` is false, `{shell:...}` is left as literal text.
pub(super) fn expand<P: Providers>(
    template: &str,
    providers: &mut P,
    snippets: &[TextSnippet],
    allow_shell: bool,
    mode: Mode,
) -> Result<Expanded, String>
where
    <P::Tz as TimeZone>::Offset: fmt::Display,
{
    let mut expander = Expander {
        providers,
        snippets,
        allow_shell,
        mode,
        stack: Vec::new(),
        snippet_expansions: 0,
    };
    let (text, cursor) = expander.expand(template)?;
    let cursor_moves = cursor.map(|position| text[position..].chars().count());
    Ok(Expanded { text, cursor_moves })
}

/// Clipboard reads a template needs: the current clipboard and/or history
/// entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ClipboardNeeds {
    pub current: bool,
    pub history: BTreeSet<usize>,
}

impl ClipboardNeeds {
    pub fn is_empty(&self) -> bool {
        !self.current && self.history.is_empty()
    }
}

/// What expanding `template` would read from the clipboard, following
/// `{snippet:...}` references. Adds to `needs`.
pub(super) fn clipboard_needs(
    template: &str,
    snippets: &[TextSnippet],
    needs: &mut ClipboardNeeds,
) {
    fn walk(
        template: &str,
        snippets: &[TextSnippet],
        seen: &mut Vec<usize>,
        needs: &mut ClipboardNeeds,
    ) {
        for piece in parse(template, false) {
            match piece {
                Piece::Token {
                    name: Name::Clipboard,
                    spec: Ok(spec),
                    ..
                } => match clipboard_index(spec.arg.as_deref()) {
                    Ok(0) => needs.current = true,
                    Ok(index) => {
                        needs.history.insert(index);
                    }
                    Err(_) => {}
                },
                Piece::Token {
                    name: Name::Snippet,
                    spec: Ok(spec),
                    ..
                } => {
                    let Some(index) =
                        find_snippet(snippets, spec.arg.as_deref().unwrap_or_default())
                    else {
                        continue;
                    };
                    if seen.contains(&index) {
                        continue;
                    }
                    seen.push(index);
                    walk(&snippets[index].value, snippets, seen, needs);
                }
                _ => {}
            }
        }
    }
    walk(template, snippets, &mut Vec::new(), needs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, MappedLocalTime, NaiveDate, NaiveDateTime};

    /// A zone at UTC+1 that jumps to UTC+2 on 2026-03-29 at 01:00 UTC, so
    /// local 02:00..03:00 that day does not exist.
    #[derive(Debug, Clone, Copy)]
    struct DstZone;

    fn switch_utc() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(1, 0, 0)
            .unwrap()
    }

    impl TimeZone for DstZone {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            DstZone
        }

        fn offset_from_local_date(&self, _: &NaiveDate) -> MappedLocalTime<FixedOffset> {
            MappedLocalTime::Single(FixedOffset::east_opt(3600).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<FixedOffset> {
            let winter = FixedOffset::east_opt(3600).unwrap();
            let summer = FixedOffset::east_opt(7200).unwrap();
            let as_winter = *local - TimeDelta::hours(1) < switch_utc();
            let as_summer = *local - TimeDelta::hours(2) >= switch_utc();
            match (as_winter, as_summer) {
                (true, false) => MappedLocalTime::Single(winter),
                (false, true) => MappedLocalTime::Single(summer),
                (true, true) => MappedLocalTime::Ambiguous(winter, summer),
                (false, false) => MappedLocalTime::None,
            }
        }

        fn offset_from_utc_date(&self, _: &NaiveDate) -> FixedOffset {
            FixedOffset::east_opt(3600).unwrap()
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            if *utc < switch_utc() {
                FixedOffset::east_opt(3600).unwrap()
            } else {
                FixedOffset::east_opt(7200).unwrap()
            }
        }
    }

    struct Fake {
        now: DateTime<DstZone>,
        clipboard: Result<String, String>,
        history: Vec<&'static str>,
        history_calls: Vec<usize>,
        shell_calls: Vec<String>,
        random: Vec<u64>,
        random_calls: usize,
    }

    impl Fake {
        fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Self {
            Self {
                now: DstZone
                    .with_ymd_and_hms(year, month, day, hour, minute, 0)
                    .single()
                    .unwrap(),
                clipboard: Ok("Hello World".into()),
                history: vec!["Hello World", "previous", "older"],
                history_calls: Vec::new(),
                shell_calls: Vec::new(),
                random: vec![7],
                random_calls: 0,
            }
        }
    }

    impl Providers for Fake {
        type Tz = DstZone;

        fn now(&mut self) -> DateTime<DstZone> {
            self.now
        }

        fn clipboard(&mut self) -> Result<String, String> {
            self.clipboard.clone()
        }

        fn clipboard_history(&mut self, index: usize) -> Result<String, String> {
            self.history_calls.push(index);
            self.history
                .get(index)
                .map(|entry| entry.to_string())
                .ok_or_else(|| format!("only {} entries", self.history.len()))
        }

        fn env(&mut self, name: &str) -> Option<String> {
            (name == "USER").then(|| "chmouel".to_string())
        }

        fn shell(&mut self, command: &str) -> Result<String, String> {
            self.shell_calls.push(command.to_string());
            Ok(format!("ran<{command}>"))
        }

        fn random_u64(&mut self) -> Result<u64, String> {
            let value = self.random[self.random_calls % self.random.len()];
            self.random_calls += 1;
            Ok(value)
        }
    }

    fn snippet(name: &str, value: &str, keyword: Option<&str>) -> TextSnippet {
        TextSnippet {
            name: name.into(),
            value: value.into(),
            keyword: keyword.map(Into::into),
        }
    }

    fn run(template: &str) -> Result<String, String> {
        run_with(&mut Fake::at(2026, 1, 31, 9, 5), template, &[])
    }

    fn run_with(
        fake: &mut Fake,
        template: &str,
        snippets: &[TextSnippet],
    ) -> Result<String, String> {
        expand(template, fake, snippets, true, Mode::Final).map(|expanded| expanded.text)
    }

    #[test]
    fn test_dates_and_formats() {
        assert_eq!(run("{date}").unwrap(), "2026-01-31");
        assert_eq!(run("{time}").unwrap(), "09:05");
        assert_eq!(run("{datetime}").unwrap(), "2026-01-31 09:05");
        assert_eq!(run("{date:%A %d}").unwrap(), "Saturday 31");
        assert_eq!(run("{time:%H:%M:%S}").unwrap(), "09:05:00");
        assert!(run("{date:%Q}")
            .unwrap_err()
            .contains("invalid date format"));
    }

    #[test]
    fn test_date_offsets() {
        assert_eq!(run("{date +1d}").unwrap(), "2026-02-01");
        assert_eq!(run("{date -1w}").unwrap(), "2026-01-24");
        assert_eq!(run("{date +1M}").unwrap(), "2026-02-28");
        assert_eq!(run("{date -1y +1M:%d/%m/%Y}").unwrap(), "28/02/2025");
        assert_eq!(run("{datetime -2h +30m}").unwrap(), "2026-01-31 07:35");
        let mut leap = Fake::at(2028, 1, 31, 12, 0);
        assert_eq!(
            run_with(&mut leap, "{date +1M}", &[]).unwrap(),
            "2028-02-29"
        );
        assert!(run("{date +1x}").is_err());
        assert!(run("{date 1d}").is_err());
        assert!(run("{date +4294967295y}")
            .unwrap_err()
            .contains("out of range"));
        assert!(run("{env +1d:USER}").is_err());
    }

    #[test]
    fn test_date_offsets_across_daylight_saving() {
        let mut fake = Fake::at(2026, 3, 28, 2, 30);
        let error = run_with(&mut fake, "{datetime +1d}", &[]).unwrap_err();
        assert!(error.contains("daylight saving gap"), "{error}");
        // Elapsed hours follow the clock change: 00:30 +2h lands on 03:30.
        let mut fake = Fake::at(2026, 3, 29, 0, 30);
        assert_eq!(run_with(&mut fake, "{time +2h}", &[]).unwrap(), "03:30");
    }

    #[test]
    fn test_clipboard_env_and_transforms() {
        assert_eq!(run("<{clipboard}>").unwrap(), "<Hello World>");
        assert_eq!(run("{clipboard|uppercase}").unwrap(), "HELLO WORLD");
        assert_eq!(
            run("{clipboard|lowercase|capitals}").unwrap(),
            "Hello World"
        );
        assert_eq!(run("{env:USER|uppercase}").unwrap(), "CHMOUEL");
        assert_eq!(run("[{env:NOPE}]").unwrap(), "[]");
        assert!(run("{clipboard|shout}")
            .unwrap_err()
            .contains("unknown transform"));
        assert!(run("{clipboard:x}").is_err());
        assert!(run("{env:}").is_err());

        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        fake.clipboard = Ok("  padded\n".into());
        assert_eq!(
            run_with(&mut fake, "{clipboard}", &[]).unwrap(),
            "  padded\n"
        );
        assert_eq!(
            run_with(&mut fake, "{clipboard|trim}", &[]).unwrap(),
            "padded"
        );

        fake.clipboard = Err("empty".into());
        assert!(run_with(&mut fake, "{clipboard}", &[]).is_err());
    }

    #[test]
    fn test_expansion_is_single_pass() {
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        fake.clipboard = Ok("{date} {shell:rm -rf ~}".into());
        assert_eq!(
            run_with(&mut fake, "{clipboard}", &[]).unwrap(),
            "{date} {shell:rm -rf ~}"
        );
        assert!(fake.shell_calls.is_empty());
    }

    #[test]
    fn test_literal_braces_and_escapes() {
        assert_eq!(run("fn main() { }").unwrap(), "fn main() { }");
        assert_eq!(
            run("{{ .Name }} {value} {Date} { date}").unwrap(),
            "{{ .Name }} {value} {Date} { date}"
        );
        assert_eq!(run("open {date").unwrap(), "open {date");
        assert_eq!(run("\\{date} is {date}").unwrap(), "{date} is 2026-01-31");
        assert_eq!(run("C:\\path \\{nope}").unwrap(), "C:\\path \\{nope}");
        assert_eq!(run("{date}{time}").unwrap(), "2026-01-3109:05");
        assert_eq!(run("é{date}ü").unwrap(), "é2026-01-31ü");
        assert_eq!(
            run("if x {\n  log(\"{date}\")\n}").unwrap(),
            "if x {\n  log(\"2026-01-31\")\n}"
        );
    }

    #[test]
    fn test_shell() {
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        assert_eq!(
            run_with(&mut fake, "a {shell:printf '%s' '\\}' | tr a b} z", &[]).unwrap(),
            "a ran<printf '%s' '}' | tr a b> z"
        );
        assert!(run_with(&mut fake, "{shell:}", &[]).is_err());
        assert!(run_with(&mut fake, "{shell:echo", &[]).unwrap() == "{shell:echo");

        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        let text = expand("{shell:id}", &mut fake, &[], false, Mode::Final)
            .unwrap()
            .text;
        assert_eq!(text, "{shell:id}");
        assert!(fake.shell_calls.is_empty());

        // Escaping gives the same literal whether or not shell is allowed.
        for allow_shell in [true, false] {
            let text = expand("\\{shell:id}", &mut fake, &[], allow_shell, Mode::Final)
                .unwrap()
                .text;
            assert_eq!(text, "{shell:id}");
        }
        assert!(fake.shell_calls.is_empty());
    }

    #[test]
    fn test_preview_never_runs_shell_or_random() {
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        let preview = expand(
            "{date} {shell:id} {random:uuid}",
            &mut fake,
            &[],
            true,
            Mode::Preview,
        )
        .unwrap();
        assert_eq!(preview.text, "2026-01-01 {shell:id} {random:uuid}");
        assert!(fake.shell_calls.is_empty());
        assert_eq!(fake.random_calls, 0);
    }

    #[test]
    fn test_random() {
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        fake.random = vec![u64::MAX, 41];
        // u64::MAX is rejected to avoid modulo bias, 41 % 100 + 1 = 42.
        assert_eq!(run_with(&mut fake, "{random:1-100}", &[]).unwrap(), "42");
        assert_eq!(fake.random_calls, 2);

        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        fake.random = vec![123];
        assert_eq!(
            run_with(&mut fake, "{random:0-18446744073709551615}", &[]).unwrap(),
            "123"
        );
        assert_eq!(run_with(&mut fake, "{random:5-5}", &[]).unwrap(), "5");

        assert!(run("{random:10-1}")
            .unwrap_err()
            .contains("start is above end"));
        assert!(run("{random:-1-5}").is_err());
        assert!(run("{random:1-99999999999999999999}").is_err());
        assert!(run("{random}").is_err());

        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        fake.random = vec![u64::MAX];
        let uuid = run_with(&mut fake, "{random:uuid}", &[]).unwrap();
        assert_eq!(uuid, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        fake.random = vec![0];
        let uuid = run_with(&mut fake, "{random:uuid|uppercase}", &[]).unwrap();
        assert_eq!(uuid, "00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn test_snippet_references() {
        let snippets = vec![
            snippet("Sig", "-- {env:USER}{cursor}", Some("name-or-keyword")),
            snippet("name-or-keyword", "by name", None),
            snippet("Loop A", "{snippet:Loop B}", None),
            snippet("Loop B", "{snippet:Loop A}", None),
            snippet("Clip", "[{clipboard}]", Some("cl")),
        ];
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        let expanded = expand(
            "Hi{cursor}\n{snippet:Sig|uppercase}",
            &mut fake,
            &snippets,
            true,
            Mode::Final,
        )
        .unwrap();
        assert_eq!(expanded.text, "Hi\n-- CHMOUEL");
        assert_eq!(expanded.cursor_moves, Some(11));

        assert_eq!(
            run_with(&mut fake, "{snippet:name-or-keyword}", &snippets).unwrap(),
            "by name"
        );
        assert_eq!(
            run_with(&mut fake, "{snippet:cl}", &snippets).unwrap(),
            "[Hello World]"
        );
        assert!(run_with(&mut fake, "{snippet:Missing}", &snippets)
            .unwrap_err()
            .contains("no snippet"));
        assert!(run_with(&mut fake, "{snippet:Loop A}", &snippets)
            .unwrap_err()
            .contains("includes itself"));

        let chain: Vec<TextSnippet> = (0..8)
            .map(|index| {
                snippet(
                    &format!("s{index}"),
                    &format!("{{snippet:s{}}}", index + 1),
                    None,
                )
            })
            .collect();
        assert!(run_with(&mut fake, "{snippet:s0}", &chain)
            .unwrap_err()
            .contains("too deeply"));

        let needs = |template: &str| {
            let mut needs = ClipboardNeeds::default();
            clipboard_needs(template, &snippets, &mut needs);
            needs
        };
        assert!(needs("{snippet:cl}").current);
        assert!(needs("{snippet:Sig}").is_empty());
        assert!(needs("{snippet:Loop A}").is_empty());
        assert!(needs("\\{clipboard}").is_empty());
    }

    #[test]
    fn test_snippet_fan_out_is_bounded() {
        // Each level references the next one ten times: 10^5 expansions
        // without a total limit, well within the depth limit.
        let fan_out: Vec<TextSnippet> = (0..5)
            .map(|index| {
                let value = if index == 4 {
                    "x".to_string()
                } else {
                    format!("{{snippet:f{}}}", index + 1).repeat(10)
                };
                snippet(&format!("f{index}"), &value, None)
            })
            .collect();
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        for mode in [Mode::Final, Mode::Preview] {
            let error = expand("{snippet:f0}", &mut fake, &fan_out, true, mode).unwrap_err();
            assert!(error.contains("more than 1000 times"), "{error}");
        }
    }

    #[test]
    fn test_clipboard_history() {
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        assert_eq!(
            run_with(&mut fake, "{clipboard:0}", &[]).unwrap(),
            "Hello World"
        );
        assert_eq!(
            run_with(&mut fake, "{clipboard:1} / { clipboard }", &[]).unwrap(),
            "previous / { clipboard }"
        );
        assert_eq!(
            run_with(&mut fake, "{clipboard:2|uppercase}", &[]).unwrap(),
            "OLDER"
        );
        assert_eq!(fake.history_calls, vec![1, 2]);
        assert!(run_with(&mut fake, "{clipboard:3}", &[])
            .unwrap_err()
            .contains("history entry 3: only 3 entries"));
        assert!(run("{clipboard:101}").unwrap_err().contains("0 to 100"));
        assert!(run("{clipboard:+1}").is_err());

        let snippets = vec![snippet("Prev", "{clipboard:1}", None)];
        let mut needs = ClipboardNeeds::default();
        clipboard_needs(
            "{clipboard} {snippet:Prev} {clipboard:4|trim}",
            &snippets,
            &mut needs,
        );
        assert!(needs.current);
        assert_eq!(needs.history.into_iter().collect::<Vec<_>>(), vec![1, 4]);
    }

    #[test]
    fn test_output_budget() {
        let big = "x".repeat(MAX_VALUE / 4);
        let snippets = vec![snippet("big", &big, None)];
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        assert!(run_with(&mut fake, "{snippet:big}{snippet:big}", &snippets).is_ok());
        let template = "{snippet:big}".repeat(5);
        assert!(run_with(&mut fake, &template, &snippets)
            .unwrap_err()
            .contains("larger than"));
    }

    #[test]
    fn test_cursor() {
        let mut fake = Fake::at(2026, 1, 1, 0, 0);
        let expanded = expand("ab{cursor}é😀{cursor}c", &mut fake, &[], true, Mode::Final).unwrap();
        assert_eq!(expanded.text, "abé😀c");
        assert_eq!(expanded.cursor_moves, Some(3));

        let expanded = expand("no cursor", &mut fake, &[], true, Mode::Final).unwrap();
        assert_eq!(expanded.cursor_moves, None);
        assert!(run("{cursor|trim}").is_err());
    }
}
