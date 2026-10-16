use std::fs::File;
use std::io::Read;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime, SecondsFormat, TimeZone, Utc};

use super::actions::run_bounded_with_input;
use super::preview::PreviewData;
use super::types::{InlineProvider, InlineResult, InlineRow};

pub(super) const MIN_PASSWORD_LENGTH: usize = 8;
pub(super) const MAX_PASSWORD_LENGTH: usize = 128;

const PASSWORD_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&*+-=?@^_~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Generator {
    Uuid,
    Password(usize),
}

/// Matches `uuid`, `pw` and `pw <length>` exactly, so app names that merely
/// start with them are left alone. Lengths are clamped to 8..=128.
pub(super) fn try_parse_generator(query: &str, default_length: usize) -> Option<Generator> {
    let query = query.trim();
    if query.eq_ignore_ascii_case("uuid") {
        return Some(Generator::Uuid);
    }
    let (keyword, length) = match query.split_once(char::is_whitespace) {
        Some((keyword, rest)) => (keyword, Some(rest.trim_start())),
        None => (query, None),
    };
    if !keyword.eq_ignore_ascii_case("pw") {
        return None;
    }
    let length = match length {
        None => default_length,
        Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()?
        }
        Some(_) => return None,
    };
    Some(Generator::Password(
        length.clamp(MIN_PASSWORD_LENGTH, MAX_PASSWORD_LENGTH),
    ))
}

/// Generates a fresh value; called once per query change.
pub(super) fn generator_inline_result(generator: Generator) -> Option<InlineResult> {
    let (provider, value, subtitle, kind) = match generator {
        Generator::Uuid => (
            InlineProvider::Uuid,
            generate_uuid()?,
            "Random UUID (v4)".to_string(),
            "UUID",
        ),
        Generator::Password(length) => (
            InlineProvider::Password,
            generate_password(length, &mut File::open("/dev/urandom").ok()?)?,
            format!("Random password, {length} characters"),
            "Password",
        ),
    };
    let preview = PreviewData {
        title: kind.to_string(),
        subtitle: Some(subtitle.clone()),
        badge: Some("Generator".to_string()),
        icon: None,
        bodies: Vec::new(),
        metadata: vec![
            ("Enter".to_string(), "Copy to clipboard".to_string()),
            ("History".to_string(), "Never saved".to_string()),
        ],
    };
    Some(InlineResult {
        provider,
        rows: vec![InlineRow {
            title: value.clone(),
            subtitle: Some(subtitle),
            copy_value: value,
            alt_copy_value: None,
            swatch: None,
            preview: Some(preview),
        }],
    })
}

fn generate_uuid() -> Option<String> {
    if let Ok(uuid) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let uuid = uuid.trim().to_ascii_lowercase();
        if is_uuid_v4(&uuid) {
            return Some(uuid);
        }
    }
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut bytes)
        .ok()?;
    Some(uuid_from_bytes(bytes))
}

pub(super) fn uuid_from_bytes(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub(super) fn is_uuid_v4(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, &b)| match index {
            8 | 13 | 18 | 23 => b == b'-',
            14 => b == b'4',
            19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
            _ => b.is_ascii_hexdigit() && !b.is_ascii_uppercase(),
        })
}

/// Rejection sampling keeps every character equally likely: bytes at or
/// above the largest multiple of the alphabet size are discarded.
pub(super) fn generate_password(length: usize, random: &mut impl Read) -> Option<String> {
    let limit = 256 - 256 % PASSWORD_CHARS.len();
    let mut password = String::with_capacity(length);
    let mut buffer = [0u8; 64];
    while password.len() < length {
        random.read_exact(&mut buffer).ok()?;
        for &byte in buffer.iter().filter(|&&byte| usize::from(byte) < limit) {
            password.push(char::from(
                PASSWORD_CHARS[usize::from(byte) % PASSWORD_CHARS.len()],
            ));
            if password.len() == length {
                break;
            }
        }
    }
    Some(password)
}

#[cfg(test)]
pub(super) fn is_password_char(c: char) -> bool {
    c.is_ascii() && PASSWORD_CHARS.contains(&(c as u8))
}

/// One row of the `dev` list. Rows that are not `copyable` show a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DevRow {
    pub title: &'static str,
    pub value: String,
    pub copyable: bool,
}

impl DevRow {
    pub fn copy(title: &'static str, value: String) -> Self {
        Self {
            title,
            value,
            copyable: true,
        }
    }

    pub fn status(title: &'static str, value: impl Into<String>) -> Self {
        Self {
            title,
            value: value.into(),
            copyable: false,
        }
    }
}

/// Hash rows: title and the coreutils program computing it.
pub(super) const HASHES: [(&str, &str); 3] = [
    ("SHA-256", "sha256sum"),
    ("SHA-1", "sha1sum"),
    ("MD5", "md5sum"),
];

pub(super) const HASH_PENDING: &str = "Computing...";
pub(super) const HASH_DEBOUNCE: Duration = Duration::from_millis(150);
const HASH_TIMEOUT: Duration = Duration::from_secs(2);

/// Rows computed in place for `payload`, which is used exactly as typed.
/// Transforms that fail on the input or would not change it are left out.
pub(super) fn transform_rows(payload: &str) -> Vec<DevRow> {
    if payload.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut push = |title, value: Option<String>| {
        if let Some(value) = value.filter(|value| value != payload) {
            rows.push(DevRow::copy(title, value));
        }
    };
    push("Base64 encode", Some(base64_encode(payload.as_bytes())));
    push(
        "Base64 decode",
        base64_decode(payload).and_then(printable_text),
    );
    push("URL encode", Some(url_encode(payload)));
    push("URL decode", url_decode(payload).and_then(printable_text));
    let epoch = epoch_to_date(payload);
    push(
        "Epoch to local date",
        epoch.as_ref().map(|(local, _)| local.clone()),
    );
    push("Epoch to UTC date", epoch.map(|(_, utc)| utc));
    push("Date to epoch", date_to_epoch(payload));
    let is_json = is_json_container(payload);
    push("JSON pretty", is_json.then(|| json_reformat(payload, true)));
    push(
        "JSON minify",
        is_json.then(|| json_reformat(payload, false)),
    );
    push("Uppercase", Some(payload.to_uppercase()));
    push("Lowercase", Some(payload.to_lowercase()));
    rows
}

/// Runs a hash tool such as `sha256sum` on `input` and returns its digest.
/// Errors never contain the input or the tool output.
pub(super) fn run_hash(argv: &[&str], input: &str) -> Result<String, String> {
    let output = run_bounded_with_input(argv, input.as_bytes(), HASH_TIMEOUT, 4096)?;
    let output = String::from_utf8_lossy(&output);
    output
        .split_whitespace()
        .next()
        .filter(|digest| digest.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "unexpected output".to_string())
}

/// Runs every hash job on `input`. A failure becomes a status row whose
/// text never contains the input.
pub(super) fn compute_hash_rows(jobs: &[(&'static str, Vec<String>)], input: &str) -> Vec<DevRow> {
    jobs.iter()
        .map(|(title, argv)| {
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            match run_hash(&argv, input) {
                Ok(digest) => DevRow::copy(title, digest),
                Err(error) => DevRow::status(title, format!("Hash failed: {error}")),
            }
        })
        .collect()
}

fn printable_text(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    let printable = text
        .chars()
        .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'));
    (printable && !text.is_empty()).then_some(text)
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(super) fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let group = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(
                    BASE64_ALPHABET[(group >> shift) as usize & 0x3f],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes standard or URL-safe base64, with or without padding. Input with
/// any other character, a wrong length or non-zero trailing bits is rejected.
pub(super) fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let data = input.trim_end_matches('=');
    let padding = input.len() - data.len();
    if data.is_empty() || data.len() % 4 == 1 || padding > 2 {
        return None;
    }
    if padding > 0 && !input.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for byte in data.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

/// Percent-encodes everything except the RFC 3986 unreserved characters.
pub(super) fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Decodes `%XX` sequences; `+` is kept as is. A stray `%` makes it fail.
pub(super) fn url_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Some(out)
}

/// Values from 10^11 up are taken as milliseconds.
fn epoch_to_date(payload: &str) -> Option<(String, String)> {
    let number: i64 = payload.trim().parse().ok()?;
    let (utc, format) = if number.unsigned_abs() >= 100_000_000_000 {
        (
            DateTime::<Utc>::from_timestamp_millis(number)?,
            SecondsFormat::Millis,
        )
    } else {
        (
            DateTime::<Utc>::from_timestamp(number, 0)?,
            SecondsFormat::Secs,
        )
    };
    let local = utc.with_timezone(&Local);
    let local = if format == SecondsFormat::Millis {
        local.format("%Y-%m-%d %H:%M:%S%.3f %:z")
    } else {
        local.format("%Y-%m-%d %H:%M:%S %:z")
    };
    Some((local.to_string(), utc.to_rfc3339_opts(format, true)))
}

/// Accepts RFC 3339 or `YYYY-MM-DD HH:MM:SS` in local time.
fn date_to_epoch(payload: &str) -> Option<String> {
    let text = payload.trim();
    if let Ok(date) = DateTime::parse_from_rfc3339(text) {
        return Some(date.timestamp().to_string());
    }
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|date| date.timestamp().to_string())
}

fn is_json_container(payload: &str) -> bool {
    let trimmed = payload.trim_start();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde::de::IgnoredAny>(payload).is_ok()
}

/// Re-indents valid JSON text without parsing it into values, so key order
/// and number spelling are kept.
fn json_reformat(input: &str, pretty: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = input.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        if pretty {
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
        }
    };
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ' ' | '\t' | '\n' | '\r' => {}
            '{' | '[' => {
                out.push(c);
                while chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                    chars.next();
                }
                if let Some(close) = chars.next_if(|&next| next == '}' || next == ']') {
                    out.push(close);
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => {
                out.push(c);
                if pretty {
                    out.push(' ');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(rows: &[DevRow]) -> Vec<&'static str> {
        rows.iter().map(|row| row.title).collect()
    }

    fn value<'a>(rows: &'a [DevRow], title: &str) -> Option<&'a str> {
        rows.iter()
            .find(|row| row.title == title)
            .map(|row| row.value.as_str())
    }

    #[test]
    fn test_base64_rfc4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            if !plain.is_empty() {
                assert_eq!(base64_decode(encoded).unwrap(), plain.as_bytes());
                let unpadded = encoded.trim_end_matches('=');
                assert_eq!(base64_decode(unpadded).unwrap(), plain.as_bytes());
            }
        }
        assert_eq!(base64_decode("-_8").unwrap(), [0xfb, 0xff]);
        assert_eq!(base64_decode("+/8=").unwrap(), [0xfb, 0xff]);
        for invalid in ["Zg=", "Zg===", "Z", "Zh==", "Zm9v!", "Zm 9v", "=", "Zm=9v"] {
            assert_eq!(base64_decode(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn test_url_encoding_round_trip() {
        assert_eq!(url_encode("a b&c=d/é~"), "a%20b%26c%3Dd%2F%C3%A9~");
        assert_eq!(
            url_decode("a%20b%26c%3dd%2F%C3%A9+").unwrap(),
            "a b&c=d/é+".as_bytes()
        );
        assert_eq!(url_decode("100%"), None);
        assert_eq!(url_decode("%zz"), None);
        assert_eq!(url_decode("%e"), None);
    }

    #[test]
    fn test_transform_rows_use_the_raw_payload() {
        let rows = transform_rows(" hi ");
        assert_eq!(value(&rows, "Base64 encode"), Some("IGhpIA=="));
        assert_eq!(value(&rows, "URL encode"), Some("%20hi%20"));
        assert_eq!(value(&rows, "Uppercase"), Some(" HI "));
        assert_eq!(value(&transform_rows("hi"), "Base64 encode"), Some("aGk="));
        assert!(transform_rows("").is_empty());
    }

    #[test]
    fn test_transform_rows_leave_out_failing_and_no_op_transforms() {
        let rows = transform_rows("hello");
        assert_eq!(titles(&rows), ["Base64 encode", "Uppercase"]);

        // Valid base64 that decodes to non-UTF-8 or control bytes.
        assert_eq!(value(&transform_rows("/w=="), "Base64 decode"), None);
        assert_eq!(value(&transform_rows("AAE="), "Base64 decode"), None);
        assert_eq!(
            value(&transform_rows("aGVsbG8="), "Base64 decode"),
            Some("hello")
        );
        assert_eq!(value(&transform_rows("%ff"), "URL decode"), None);
        assert_eq!(value(&transform_rows("a%2Fb"), "URL decode"), Some("a/b"));

        let bad_json = transform_rows("{\"a\": }");
        assert_eq!(value(&bad_json, "JSON pretty"), None);
        assert_eq!(value(&bad_json, "JSON minify"), None);
        assert_eq!(value(&transform_rows("42"), "JSON pretty"), None);
    }

    #[test]
    fn test_json_reformat_keeps_order_and_strings() {
        let input = r#"{ "z": [1, 2.50, {}], "a": "x, \"y\": [z]", "e": [ ] }"#;
        let rows = transform_rows(input);
        assert_eq!(
            value(&rows, "JSON minify"),
            Some(r#"{"z":[1,2.50,{}],"a":"x, \"y\": [z]","e":[]}"#)
        );
        assert_eq!(
            value(&rows, "JSON pretty"),
            Some("{\n  \"z\": [\n    1,\n    2.50,\n    {}\n  ],\n  \"a\": \"x, \\\"y\\\": [z]\",\n  \"e\": []\n}")
        );
        // Already minified input only offers the pretty form.
        let minified = transform_rows(r#"{"a":1}"#);
        assert_eq!(value(&minified, "JSON minify"), None);
        assert_eq!(value(&minified, "JSON pretty"), Some("{\n  \"a\": 1\n}"));
    }

    #[test]
    fn test_epoch_and_date_rows() {
        let rows = transform_rows("1700000000");
        assert_eq!(
            value(&rows, "Epoch to UTC date"),
            Some("2023-11-14T22:13:20Z")
        );
        assert!(value(&rows, "Epoch to local date").is_some());
        assert_eq!(
            value(&transform_rows("1700000000123"), "Epoch to UTC date"),
            Some("2023-11-14T22:13:20.123Z")
        );
        assert_eq!(
            value(&transform_rows("2023-11-14T22:13:20Z"), "Date to epoch"),
            Some("1700000000")
        );
        assert_eq!(
            value(
                &transform_rows("2023-11-14T23:13:20+01:00"),
                "Date to epoch"
            ),
            Some("1700000000")
        );
        assert!(value(&transform_rows("2023-11-14 22:13:20"), "Date to epoch").is_some());
        assert_eq!(
            value(&transform_rows("2023-13-14 22:13:20"), "Date to epoch"),
            None
        );
        assert_eq!(value(&transform_rows("12abc"), "Epoch to UTC date"), None);
    }

    #[test]
    fn test_generator_parsing() {
        for (query, expected) in [
            ("uuid", Some(Generator::Uuid)),
            (" UUID ", Some(Generator::Uuid)),
            ("pw", Some(Generator::Password(20))),
            ("pw 32", Some(Generator::Password(32))),
            ("pw 3", Some(Generator::Password(8))),
            ("pw 1000", Some(Generator::Password(128))),
            ("pw ", Some(Generator::Password(20))),
            ("uuidgen", None),
            ("pwgen", None),
            ("pw abc", None),
            ("pw 12 x", None),
            ("pw -5", None),
            ("pwd", None),
        ] {
            assert_eq!(try_parse_generator(query, 20), expected, "{query}");
        }
    }

    #[test]
    fn test_password_generation_bounds_and_charset() {
        let mut random = File::open("/dev/urandom").unwrap();
        for length in [MIN_PASSWORD_LENGTH, 20, MAX_PASSWORD_LENGTH] {
            let password = generate_password(length, &mut random).unwrap();
            assert_eq!(password.chars().count(), length);
            assert!(password.chars().all(is_password_char), "{password}");
        }

        // Bytes past the largest multiple of the alphabet size are skipped.
        let limit = 256 - 256 % PASSWORD_CHARS.len();
        let mut bytes = vec![255u8; 64];
        bytes[10] = 0;
        bytes[20] = (limit - 1) as u8;
        bytes.extend([1u8; 64]);
        let password = generate_password(4, &mut std::io::Cursor::new(bytes)).unwrap();
        let last = char::from(PASSWORD_CHARS[(limit - 1) % PASSWORD_CHARS.len()]);
        assert_eq!(password, format!("A{last}BB"));

        // A short random source gives no password rather than a weak one.
        assert_eq!(
            generate_password(8, &mut std::io::Cursor::new(vec![0u8; 10])),
            None
        );
    }

    #[test]
    fn test_uuid_format() {
        let uuid = uuid_from_bytes([0xff; 16]);
        assert_eq!(uuid, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert!(is_uuid_v4(&uuid));
        assert!(is_uuid_v4(&uuid_from_bytes([0; 16])));
        assert!(is_uuid_v4(&generate_uuid().unwrap()));
        assert!(!is_uuid_v4("ffffffff-ffff-1fff-bfff-ffffffffffff"));
        assert!(!is_uuid_v4("FFFFFFFF-FFFF-4FFF-BFFF-FFFFFFFFFFFF"));
    }

    #[test]
    fn test_run_hash_parses_digest_and_hides_failures() {
        let digest = run_hash(&["sh", "-c", "cat >/dev/null; echo 'ABC123  -'"], "x");
        assert_eq!(digest.as_deref(), Ok("abc123"));
        let echoed = run_hash(&["sh", "-c", "cat; exit 0"], "secret");
        assert_eq!(echoed, Err("unexpected output".into()));
        let failed = run_hash(&["sh", "-c", "cat >&2; exit 3"], "secret").unwrap_err();
        assert_eq!(failed, "sh failed (exit 3)");
        let missing = run_hash(&["raffi-no-such-hash-tool"], "secret").unwrap_err();
        assert!(!missing.contains("secret"), "{missing}");
    }
}
