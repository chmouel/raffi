//! Clipboard history browser backed by CopyQ.
//!
//! CopyQ owns the history; Raffi reads and republishes items only through
//! `copyq eval` scripts. Rows move whenever the history changes, so every item
//! is identified by a fingerprint of all its formats. Previews and copies look
//! the fingerprint up again, inside the same script that uses the item, and
//! refuse to act when it is gone.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use chrono::{Datelike, NaiveDateTime};
use serde::Deserialize;

use super::actions::run_bounded;

const PRELUDE: &str = include_str!("copyq/prelude.js");
const LIST_SCRIPT: &str = include_str!("copyq/list.js");
const PREVIEW_SCRIPT: &str = include_str!("copyq/preview.js");
const SELECT_SCRIPT: &str = include_str!("copyq/select.js");
const DELETE_SCRIPT: &str = include_str!("copyq/delete.js");

/// Rows scanned per `copyq eval` while listing or searching.
pub(super) const PAGE_SIZE: usize = 400;
/// Characters of text sent back for each row label.
const EXCERPT_CHARS: usize = 300;
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// Text shown in the preview pane, matching the file preview limits.
const PREVIEW_TEXT_BYTES: usize = super::preview::MAX_FILE_PREVIEW_BYTES;
const PREVIEW_TEXT_LINES: usize = super::preview::MAX_FILE_PREVIEW_LINES;
/// Largest encoded image fetched for a preview.
pub(super) const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Largest decoded image shown, to bound the memory iced allocates.
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PREVIEW_FILES: usize = 50;
/// How long CopyQ may take to own the clipboard after `select()`.
const SELECT_VERIFY_MS: u64 = 1500;
const SELECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ItemKind {
    Text,
    Image,
    Files,
    Html,
    Other,
}

impl ItemKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Image => "Image",
            Self::Files => "Files",
            Self::Html => "Rich text",
            Self::Other => "Data",
        }
    }
}

/// One history entry as listed by CopyQ.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ClipItem {
    /// Row when listed; only a hint, since rows shift.
    #[serde(rename = "r")]
    pub row: usize,
    /// Fingerprint of every format of the item.
    pub id: String,
    #[serde(rename = "f")]
    pub formats: Vec<String>,
    #[serde(rename = "k")]
    pub kind: ItemKind,
    /// Length of the searchable text in UTF-16 units.
    #[serde(rename = "n")]
    pub text_len: usize,
    /// Start of the searchable text.
    #[serde(rename = "p")]
    pub excerpt: String,
    /// 0: text starts with the query, 1: contains it, 2: contains every word.
    #[serde(rename = "t")]
    pub tier: u8,
    /// Local copy time, when CopyQ stored one.
    #[serde(rename = "c", default, deserialize_with = "copy_time")]
    pub copied: Option<NaiveDateTime>,
}

fn copy_time<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NaiveDateTime>, D::Error> {
    let raw = String::deserialize(deserializer)?;
    Ok(NaiveDateTime::parse_from_str(&raw, COPY_TIME_FORMAT).ok())
}

const COPY_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Short copy time for a row: the hour today, the day for the last week,
/// the date otherwise.
pub(crate) fn short_copy_time(time: NaiveDateTime, now: NaiveDateTime) -> String {
    let days = (now.date() - time.date()).num_days();
    match days {
        0 => time.format("%H:%M").to_string(),
        1 => time.format("Yesterday %H:%M").to_string(),
        2..=6 => time.format("%a %H:%M").to_string(),
        _ if time.year() == now.year() => time.format("%b %-d").to_string(),
        _ => time.format("%Y-%m-%d").to_string(),
    }
}

pub(crate) fn full_copy_time(time: NaiveDateTime) -> String {
    time.format(COPY_TIME_FORMAT).to_string()
}

impl ClipItem {
    /// Single-line label for the result row.
    pub(crate) fn title(&self) -> String {
        match self.kind {
            ItemKind::Image => image_title(&self.formats),
            ItemKind::Files => {
                let (files, partial) = self.listed_files();
                match (files.as_slice(), partial) {
                    ([], _) => "Files".to_string(),
                    ([only], false) => only.clone(),
                    ([first, ..], false) => format!("{first} (+{} more)", files.len() - 1),
                    ([first, ..], true) => format!("{first} (+{} or more)", files.len() - 1),
                }
            }
            ItemKind::Other => self
                .formats
                .iter()
                .find(|format| !format.starts_with("application/x-copyq-"))
                .cloned()
                .unwrap_or_else(|| "Empty item".to_string()),
            ItemKind::Text | ItemKind::Html => {
                let line = one_line(&self.excerpt, 160);
                if line.is_empty() {
                    "Blank text".to_string()
                } else {
                    line
                }
            }
        }
    }

    /// Type and size details for the result row.
    pub(crate) fn subtitle(&self) -> String {
        let details = self.details();
        match self.copied {
            Some(time) => {
                let now = chrono::Local::now().naive_local();
                format!("{details} · {}", short_copy_time(time, now))
            }
            None => details,
        }
    }

    fn details(&self) -> String {
        let label = self.kind.label();
        match self.kind {
            ItemKind::Text | ItemKind::Html => match self.text_len {
                1 => format!("{label} · 1 character"),
                count => format!("{label} · {count} characters"),
            },
            ItemKind::Files => match self.listed_files() {
                (files, false) if files.len() == 1 => format!("{label} · 1 entry"),
                (files, false) => format!("{label} · {} entries", files.len()),
                (files, true) => format!("{label} · {}+ entries", files.len()),
            },
            ItemKind::Image | ItemKind::Other => label.to_string(),
        }
    }

    /// Entries of a file list excerpt, and whether the excerpt was cut so
    /// that more entries may follow. A cut last line is dropped.
    fn listed_files(&self) -> (Vec<String>, bool) {
        let partial = self.excerpt.encode_utf16().count() < self.text_len;
        let mut files = parse_uri_list(&self.excerpt, usize::MAX);
        if partial && !self.excerpt.ends_with('\n') {
            files.pop();
        }
        (files, partial)
    }

    pub(crate) fn reference(&self, tab: &str) -> ItemRef {
        ItemRef {
            tab: tab.to_string(),
            id: self.id.clone(),
            row: self.row,
        }
    }
}

/// A slice of the history scanned in one request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Page {
    pub tab: String,
    pub total: usize,
    /// Fingerprint of the newest item, used to notice history changes.
    pub head: String,
    /// First row not scanned yet.
    pub next: usize,
    /// False when the history changed while this page was read.
    pub stable: bool,
    pub items: Vec<ClipItem>,
}

/// What actions and previews need to find an item again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemRef {
    pub tab: String,
    pub id: String,
    pub row: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewImage {
    pub handle: iced::widget::image::Handle,
    pub mime: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewFile {
    pub name: String,
    /// `Some(true)` for a local path that does not exist.
    pub missing: Option<bool>,
}

/// Details of the selected item for the preview pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipPreview {
    pub kind: ItemKind,
    /// Public formats with their size in bytes.
    pub formats: Vec<(String, usize)>,
    pub text: String,
    pub text_truncated: bool,
    pub image: Option<Result<PreviewImage, String>>,
    pub files: Vec<PreviewFile>,
}

#[derive(Debug, Deserialize)]
struct PreviewHeader {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "k")]
    kind: Option<ItemKind>,
    #[serde(default)]
    sizes: Vec<(String, usize)>,
    #[serde(default)]
    text: String,
    #[serde(default, rename = "n")]
    text_len: usize,
    #[serde(default)]
    image: String,
    #[serde(default)]
    sent: i64,
}

#[derive(Debug, Deserialize)]
struct SelectReply {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// CopyQ command line access, optionally to a named session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CopyQ {
    session: Option<String>,
}

impl CopyQ {
    #[cfg(test)]
    pub(crate) fn with_session(session: &str) -> Self {
        Self {
            session: Some(session.to_string()),
        }
    }

    fn eval(
        &self,
        script: &str,
        args: &[&str],
        timeout: Duration,
        max_output: usize,
    ) -> Result<Vec<u8>, String> {
        let mut command = Command::new("copyq");
        if let Some(session) = &self.session {
            command.args(["-s", session]);
        }
        command
            .arg("eval")
            .arg(format!("{PRELUDE}{script}"))
            .arg("--")
            .args(args);
        run_bounded(command, timeout, max_output).map_err(copyq_error)
    }

    /// Scan `PAGE_SIZE` rows from `start`, returning those matching `query`.
    /// An empty `tab` selects CopyQ's configured clipboard tab.
    pub(crate) fn list_page(
        &self,
        tab: &str,
        start: usize,
        query: &str,
        only_kind: &str,
        min_chars: usize,
    ) -> Result<Page, String> {
        let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
        let output = self.eval(
            LIST_SCRIPT,
            &[
                tab,
                &start.to_string(),
                &PAGE_SIZE.to_string(),
                &query,
                &EXCERPT_CHARS.to_string(),
                only_kind,
                &min_chars.to_string(),
            ],
            LIST_TIMEOUT,
            LIST_MAX_OUTPUT,
        )?;
        parse_page(&output)
    }

    pub(crate) fn preview(&self, item: &ItemRef) -> Result<ClipPreview, String> {
        let output = self.eval(
            PREVIEW_SCRIPT,
            &[
                &item.tab,
                &item.id,
                &item.row.to_string(),
                &PREVIEW_TEXT_BYTES.to_string(),
                &MAX_IMAGE_BYTES.to_string(),
            ],
            PREVIEW_TIMEOUT,
            MAX_IMAGE_BYTES + 1024 * 1024,
        )?;
        parse_preview(&output)
    }

    /// Put `item` on the clipboard with every format and confirm that the
    /// clipboard now holds it.
    pub(crate) fn select(&self, item: &ItemRef) -> Result<(), String> {
        let output = self.eval(
            SELECT_SCRIPT,
            &[
                &item.tab,
                &item.id,
                &item.row.to_string(),
                &SELECT_VERIFY_MS.to_string(),
            ],
            SELECT_TIMEOUT,
            64 * 1024,
        )?;
        let reply: SelectReply = serde_json::from_slice(&output)
            .map_err(|_| "CopyQ returned an unexpected reply".to_string())?;
        if reply.ok {
            Ok(())
        } else {
            Err(reply
                .error
                .unwrap_or_else(|| "CopyQ could not copy the item".into()))
        }
    }

    /// Remove `item` from the clipboard history.
    pub(crate) fn delete(&self, item: &ItemRef) -> Result<(), String> {
        let output = self.eval(
            DELETE_SCRIPT,
            &[&item.tab, &item.id, &item.row.to_string()],
            SELECT_TIMEOUT,
            64 * 1024,
        )?;
        let reply: SelectReply = serde_json::from_slice(&output)
            .map_err(|_| "CopyQ returned an unexpected reply".to_string())?;
        if reply.ok {
            Ok(())
        } else {
            Err(reply
                .error
                .unwrap_or_else(|| "CopyQ could not remove the item".into()))
        }
    }
}

fn copyq_error(error: String) -> String {
    if error.starts_with("cannot start") {
        "CopyQ is not installed (copyq was not found)".to_string()
    } else if error.ends_with("exit status: 1") {
        "CopyQ is not running: start the CopyQ server".to_string()
    } else if error.ends_with("exit status: 4") {
        "CopyQ rejected the request".to_string()
    } else {
        format!("copyq: {error}")
    }
}

fn parse_page(output: &[u8]) -> Result<Page, String> {
    let page: Page = serde_json::from_slice(output)
        .map_err(|_| "CopyQ returned an unexpected history listing".to_string())?;
    if page.next > page.total || page.items.iter().any(|item| item.row >= page.next) {
        return Err("CopyQ returned an inconsistent history listing".into());
    }
    Ok(page)
}

fn parse_preview(output: &[u8]) -> Result<ClipPreview, String> {
    let split = output.iter().position(|&byte| byte == b'\n');
    let (header, rest) = match split {
        Some(at) => (&output[..at], &output[at + 1..]),
        None => (output, &[][..]),
    };
    let header: PreviewHeader = serde_json::from_slice(header)
        .map_err(|_| "CopyQ returned an unexpected preview".to_string())?;
    if !header.ok {
        return Err(header
            .error
            .unwrap_or_else(|| "CopyQ could not read the item".into()));
    }
    let kind = header.kind.unwrap_or(ItemKind::Other);
    let formats: Vec<(String, usize)> = header
        .sizes
        .into_iter()
        .filter(|(format, _)| !format.starts_with("application/x-copyq-"))
        .collect();

    let image = (!header.image.is_empty()).then(|| {
        let size = formats
            .iter()
            .find(|(format, _)| *format == header.image)
            .map_or(0, |(_, size)| *size);
        if header.sent < 0 {
            return Err(format!(
                "Image is too large to preview ({}, limit {})",
                super::preview::format_byte_size(size as u64),
                super::preview::format_byte_size(MAX_IMAGE_BYTES as u64)
            ));
        }
        if rest.len() != header.sent as usize {
            return Err("CopyQ returned an incomplete image".to_string());
        }
        decode_image(&header.image, rest)
    });

    let (text, text_truncated) = match kind {
        ItemKind::Image => (String::new(), false),
        _ => bounded_text(&header.text, header.text_len),
    };
    let files = if kind == ItemKind::Files {
        inspect_files(&header.text)
    } else {
        Vec::new()
    };

    Ok(ClipPreview {
        kind,
        formats,
        text,
        text_truncated,
        image,
        files,
    })
}

fn decode_image(mime: &str, bytes: &[u8]) -> Result<PreviewImage, String> {
    let (width, height) =
        image_dimensions(bytes).ok_or_else(|| format!("Preview is not available for {mime}"))?;
    if width == 0 || height == 0 {
        return Err("Image has no pixels".into());
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(format!(
            "Image is too large to preview ({width}×{height} pixels)"
        ));
    }
    Ok(PreviewImage {
        handle: iced::widget::image::Handle::from_bytes(bytes.to_vec()),
        mime: mime.to_string(),
        width,
        height,
    })
}

/// Pixel size of a PNG, JPEG, GIF, BMP or WebP image, read from its header
/// so oversized images are refused before anything decodes them.
pub(crate) fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |at: usize| Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let le16 = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let le32 = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let le24 = |at: usize| {
        let b = bytes.get(at..at + 3)?;
        Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
    };

    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some((u32::from(le16(6)?), u32::from(le16(8)?)));
    }
    if bytes.starts_with(b"BM") {
        return if le32(14)? == 12 {
            Some((u32::from(le16(18)?), u32::from(le16(20)?)))
        } else {
            let width = i32::from_le_bytes(bytes.get(18..22)?.try_into().ok()?);
            let height = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
            Some((width.unsigned_abs(), height.unsigned_abs()))
        };
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return match bytes.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8 " => Some((u32::from(le16(26)? & 0x3fff), u32::from(le16(28)? & 0x3fff))),
            b"VP8L" => {
                let bits = le32(21)?;
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            _ => None,
        };
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut at = 2;
        loop {
            while bytes.get(at) == Some(&0xff) && bytes.get(at + 1) == Some(&0xff) {
                at += 1;
            }
            if *bytes.get(at)? != 0xff {
                return None;
            }
            let marker = *bytes.get(at + 1)?;
            if marker == 0x01 || (0xd0..=0xd9).contains(&marker) {
                at += 2;
                continue;
            }
            if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
                return Some((u32::from(be16(at + 7)?), u32::from(be16(at + 5)?)));
            }
            at += 2 + usize::from(be16(at + 2)?);
        }
    }
    None
}

/// Cut preview text to the preview pane limits. `full_len` is the length of
/// the whole text as reported by CopyQ.
fn bounded_text(text: &str, full_len: usize) -> (String, bool) {
    let mut truncated = text.encode_utf16().count() < full_len;
    let mut end = text.len().min(PREVIEW_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        truncated = true;
    }
    let mut kept = &text[..end];
    if let Some((at, _)) = kept.match_indices('\n').nth(PREVIEW_TEXT_LINES - 1) {
        kept = &kept[..at];
        truncated = true;
    }
    (kept.to_string(), truncated)
}

fn uri_lines(list: &str) -> impl Iterator<Item = &str> {
    list.lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Display names for the entries of a `text/uri-list`, local paths decoded.
fn parse_uri_list(list: &str, limit: usize) -> Vec<String> {
    uri_lines(list)
        .take(limit)
        .map(|uri| local_path(uri).unwrap_or_else(|| uri.to_string()))
        .collect()
}

fn local_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    super::preview::percent_decode_path(path)
}

fn inspect_files(list: &str) -> Vec<PreviewFile> {
    uri_lines(list)
        .take(MAX_PREVIEW_FILES)
        .map(|uri| match local_path(uri) {
            Some(path) => PreviewFile {
                missing: Some(!Path::new(&path).exists()),
                name: path,
            },
            None => PreviewFile {
                name: uri.to_string(),
                missing: None,
            },
        })
        .collect()
}

fn image_title(formats: &[String]) -> String {
    let format = formats
        .iter()
        .find(|format| format.starts_with("image/"))
        .map_or("image", |format| &format["image/".len()..]);
    format!("Image ({})", format.to_uppercase())
}

/// Collapse whitespace and keep at most `max_chars` characters.
/// Text without spaces uses a tighter limit so it fits in a list row.
pub(crate) fn one_line(text: &str, max_chars: usize) -> String {
    let mut line = String::new();
    for (index, word) in text.split_whitespace().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(word);
        if line.chars().count() > max_chars {
            break;
        }
    }
    let effective_max = if line.contains(' ') {
        max_chars
    } else {
        max_chars / 2
    };
    if line.chars().count() > effective_max {
        line = line.chars().take(effective_max).collect();
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(row: usize, id: &str, kind: ItemKind, excerpt: &str) -> ClipItem {
        ClipItem {
            row,
            id: id.into(),
            formats: vec!["text/plain".into()],
            kind,
            text_len: excerpt.len(),
            excerpt: excerpt.into(),
            tier: 0,
            copied: None,
        }
    }

    #[test]
    fn test_scripts_have_no_backslashes() {
        // CopyQ would expand them as escape sequences in a command-line script.
        for script in [
            PRELUDE,
            LIST_SCRIPT,
            PREVIEW_SCRIPT,
            SELECT_SCRIPT,
            DELETE_SCRIPT,
        ] {
            assert!(!script.contains('\\'));
        }
    }

    #[test]
    fn test_parse_page() {
        let output = br#"{"tab":"&clipboard","total":3,"head":"h","next":3,"stable":true,"items":[
            {"r":0,"id":"a","f":["text/plain"],"k":"text","n":12,"p":"h\u00e9llo\nworld","t":1},
            {"r":2,"id":"b","f":["image/png"],"k":"image","n":0,"p":"","t":0,"c":"2026-10-16 14:32:05"}]}"#;
        let page = parse_page(output).unwrap();
        assert_eq!(page.tab, "&clipboard");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].excerpt, "héllo\nworld");
        assert_eq!(page.items[0].tier, 1);
        assert_eq!(page.items[1].kind, ItemKind::Image);
        assert_eq!(page.items[0].copied, None);
        let copied = page.items[1].copied.unwrap();
        assert_eq!(full_copy_time(copied), "2026-10-16 14:32:05");
        assert!(page.items[1].subtitle().starts_with("Image · "));
        let odd = br#"{"tab":"t","total":1,"head":"h","next":1,"stable":true,"items":[{"r":0,"id":"a","f":[],"k":"text","n":0,"p":"","t":0,"c":"soon"}]}"#;
        assert_eq!(parse_page(odd).unwrap().items[0].copied, None);

        assert!(parse_page(b"not json").is_err());
        let beyond =
            br#"{"tab":"t","total":1,"head":"h","next":1,"stable":true,"items":[{"r":4,"id":"a","f":[],"k":"text","n":0,"p":"","t":0}]}"#;
        assert!(parse_page(beyond).is_err());
    }

    #[test]
    fn test_short_copy_time() {
        let at = |s: &str| NaiveDateTime::parse_from_str(s, COPY_TIME_FORMAT).unwrap();
        let now = at("2026-10-16 19:00:00");
        assert_eq!(short_copy_time(at("2026-10-16 08:05:00"), now), "08:05");
        assert_eq!(
            short_copy_time(at("2026-10-15 23:59:00"), now),
            "Yesterday 23:59"
        );
        assert_eq!(short_copy_time(at("2026-10-12 10:00:00"), now), "Mon 10:00");
        assert_eq!(short_copy_time(at("2026-03-02 10:00:00"), now), "Mar 2");
        assert_eq!(
            short_copy_time(at("2025-12-31 10:00:00"), now),
            "2025-12-31"
        );
    }

    #[test]
    fn test_parse_preview_text_and_errors() {
        let preview = parse_preview(
            br#"{"ok":true,"row":1,"f":["application/x-copyq-owner","text/html","text/plain"],"k":"text","sizes":[["application/x-copyq-owner",3],["text/html",20],["text/plain",5]],"n":5,"text":"hello","image":"","sent":-1}"#,
        )
        .unwrap();
        assert_eq!(preview.text, "hello");
        assert!(!preview.text_truncated);
        assert_eq!(
            preview.formats,
            [("text/html".to_string(), 20), ("text/plain".to_string(), 5)]
        );
        assert!(preview.image.is_none());

        let gone = parse_preview(br#"{"ok":false,"error":"item is gone"}"#).unwrap_err();
        assert_eq!(gone, "item is gone");
        assert!(parse_preview(b"").is_err());
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn test_parse_preview_images() {
        let header = |sent: i64| {
            format!(
                r#"{{"ok":true,"k":"image","sizes":[["image/png",29]],"n":0,"text":"","image":"image/png","sent":{sent}}}"#
            )
        };
        let mut output = header(29).into_bytes();
        output.push(b'\n');
        output.extend(png(3, 2));
        let image = parse_preview(&output).unwrap().image.unwrap().unwrap();
        assert_eq!((image.width, image.height), (3, 2));

        let mut short = header(40).into_bytes();
        short.push(b'\n');
        short.extend(png(3, 2));
        assert!(parse_preview(&short)
            .unwrap()
            .image
            .unwrap()
            .unwrap_err()
            .contains("incomplete"));

        let large = parse_preview(header(-1).as_bytes()).unwrap().image.unwrap();
        assert!(large.unwrap_err().contains("too large"));

        let mut huge = header(29).into_bytes();
        huge.push(b'\n');
        huge.extend(png(10_000, 10_000));
        assert!(parse_preview(&huge)
            .unwrap()
            .image
            .unwrap()
            .unwrap_err()
            .contains("pixels"));
    }

    #[test]
    fn test_image_dimensions() {
        assert_eq!(image_dimensions(&png(640, 480)), Some((640, 480)));
        let gif = b"GIF89a\x20\x00\x10\x00".to_vec();
        assert_eq!(image_dimensions(&gif), Some((32, 16)));
        let mut bmp = b"BM".to_vec();
        bmp.extend([0; 12]);
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(7i32.to_le_bytes());
        bmp.extend((-9i32).to_le_bytes());
        assert_eq!(image_dimensions(&bmp), Some((7, 9)));
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x20, 0x00, 0x40,
        ];
        assert_eq!(image_dimensions(&jpeg), Some((64, 32)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend([0; 8]);
        webp.extend([99, 0, 0, 49, 0, 0]);
        assert_eq!(image_dimensions(&webp), Some((100, 50)));
        assert_eq!(image_dimensions(b"<svg/>"), None);
        assert_eq!(image_dimensions(&[0xff, 0xd8, 0xff]), None);
    }

    #[test]
    fn test_bounded_text() {
        assert_eq!(bounded_text("abc", 3), ("abc".to_string(), false));
        assert_eq!(bounded_text("abc", 10), ("abc".to_string(), true));
        let long = "é".repeat(PREVIEW_TEXT_BYTES);
        let (kept, truncated) = bounded_text(&long, long.chars().count());
        assert!(truncated && kept.len() <= PREVIEW_TEXT_BYTES);
        let lines = "x\n".repeat(PREVIEW_TEXT_LINES + 5);
        let (kept, truncated) = bounded_text(&lines, lines.len());
        assert!(truncated);
        assert_eq!(kept.lines().count(), PREVIEW_TEXT_LINES);
    }

    #[test]
    fn test_titles() {
        assert_eq!(
            item(0, "a", ItemKind::Text, "  first\n\tsecond  ").title(),
            "first second"
        );
        assert_eq!(item(0, "a", ItemKind::Text, " \n").title(), "Blank text");
        let files = item(
            0,
            "f",
            ItemKind::Files,
            "# comment\nfile:///tmp/a%20b.txt\r\nhttps://example.com/x\n",
        );
        assert_eq!(files.title(), "/tmp/a b.txt (+1 more)");
        assert_eq!(files.subtitle(), "Files · 2 entries");
        let mut cut = item(0, "c", ItemKind::Files, "file:///a\nfile:///b\nfile:///c");
        cut.text_len = 500;
        assert_eq!(cut.title(), "/a (+1 or more)");
        assert_eq!(cut.subtitle(), "Files · 2+ entries");
        let mut image = item(0, "i", ItemKind::Image, "");
        image.formats = vec!["image/png".into(), "text/plain".into()];
        assert_eq!(image.title(), "Image (PNG)");
        assert_eq!(one_line(&"word ".repeat(100), 10), "word word …");
        assert_eq!(
            one_line(&"a".repeat(200), 160),
            format!("{}…", "a".repeat(80))
        );
        assert_eq!(one_line(&"b".repeat(50), 160), "b".repeat(50));
    }

    #[test]
    fn test_inspect_files_marks_missing_paths() {
        let existing = std::env::temp_dir();
        let list = format!(
            "file://{}\nfile:///definitely/not/here\nsmb://host/share\n",
            existing.display()
        );
        let files = inspect_files(&list);
        assert_eq!(files[0].missing, Some(false));
        assert_eq!(files[1].missing, Some(true));
        assert_eq!(files[2].missing, None);
    }

    #[test]
    fn test_copyq_error_messages() {
        assert!(copyq_error("cannot start: No such file".into()).contains("not installed"));
        assert!(copyq_error("exited with exit status: 1".into()).contains("not running"));
        assert!(copyq_error("timed out after 5s".into()).starts_with("copyq: timed out"));
    }

    /// Runs against a disposable CopyQ session filled with synthetic items:
    /// `copyq -s raffitest` with text, HTML, image and URI-list entries, and
    /// a text entry holding only a URL.
    #[test]
    #[ignore = "needs a running `copyq -s raffitest` session"]
    fn test_copyq_session_round_trip() {
        let copyq = CopyQ::with_session("raffitest");
        let page = copyq.list_page("", 0, "", "", 0).unwrap();
        assert!(page.stable);
        let html = page
            .items
            .iter()
            .find(|item| item.kind == ItemKind::Html)
            .expect("an HTML-only item");
        let reference = html.reference(&page.tab);
        assert_eq!(copyq.preview(&reference).unwrap().kind, ItemKind::Html);
        copyq.select(&reference).unwrap();

        if let Some(image) = page.items.iter().find(|item| item.kind == ItemKind::Image) {
            let preview = copyq.preview(&image.reference(&page.tab)).unwrap();
            assert!(preview.image.unwrap().is_ok());
        }

        let long = copyq.list_page("", 0, "", "", 3).unwrap();
        assert!(long.items.iter().all(|item| {
            !matches!(item.kind, ItemKind::Text | ItemKind::Html)
                || item.excerpt.trim().chars().count() >= 3
        }));

        let images = copyq.list_page("", 0, "", "image", 0).unwrap();
        assert!(images.items.iter().all(|item| item.kind == ItemKind::Image));
        assert_eq!(
            images.items.len(),
            page.items
                .iter()
                .filter(|item| item.kind == ItemKind::Image)
                .count()
        );

        let urls = copyq.list_page("", 0, "", "url", 0).unwrap();
        assert!(!urls.items.is_empty());
        assert!(urls.items.iter().all(|item| {
            matches!(item.kind, ItemKind::Text | ItemKind::Html)
                && !item.excerpt.trim().contains(char::is_whitespace)
        }));

        let stale = ItemRef {
            id: "0".repeat(64),
            ..reference.clone()
        };
        assert!(copyq.select(&stale).unwrap_err().contains("no longer"));
        assert!(copyq.delete(&stale).unwrap_err().contains("no longer"));

        copyq.delete(&reference).unwrap();
        let after = copyq.list_page("", 0, "", "", 0).unwrap();
        assert_eq!(after.total, page.total - 1);
        assert!(after.items.iter().all(|item| item.id != reference.id));
        assert!(copyq
            .list_page("", 0, "zz-no-match-zz", "", 0)
            .unwrap()
            .items
            .is_empty());
    }
}
