//! Clipboard history recorded by Raffi itself, for the `raffi` clipboard
//! backend.
//!
//! `raffi --cache-daemon` watches the clipboard (see `clipboard_watch`) and
//! writes every new selection here. The UI lists, previews and deletes
//! entries straight from the database and asks the daemon to put one back on
//! the clipboard, since only a running process can own the selection.
//!
//! Each entry keeps the formats it was copied with as raw bytes, so pasting
//! it later gives applications what the original source offered. Entries are
//! identified by a hash of all their formats: copying the same thing again
//! moves the existing entry to the top instead of adding a duplicate.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{Local, NaiveDateTime, TimeZone};
use rusqlite::{params, Connection, OptionalExtension};

use super::clipboard::{
    bounded_text, decode_image, inspect_files, ClipItem, ClipPreview, ItemKind, Page,
    EXCERPT_CHARS, MAX_IMAGE_BYTES, PREVIEW_TEXT_BYTES,
};

/// Tab name reported in pages; the raffi history has a single list.
pub(crate) const TAB: &str = "raffi";
/// Text formats larger than this are not recorded.
pub(crate) const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// Next history position, computed inside the statement that assigns it so
/// concurrent connections cannot hand out the same one.
const NEXT_SEQ: &str = "(SELECT COALESCE(MAX(seq), 0) + 1 FROM items)";
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const SCHEMA_VERSION: i64 = 1;

/// How much history the recorder keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub max_items: usize,
    /// Total bytes of every format of every entry.
    pub max_size: u64,
    /// Largest single image format recorded.
    pub max_image_size: u64,
}

impl Limits {
    pub(crate) fn from_config(config: &crate::ClipboardAddonConfig) -> Self {
        Self {
            max_items: config.max_items(),
            max_size: config.max_size_bytes(),
            max_image_size: config.max_image_size_bytes(),
        }
    }
}

/// One clipboard selection to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    /// MIME type and bytes of each recorded format.
    pub formats: Vec<(String, Vec<u8>)>,
    /// Unix time of the copy.
    pub copied_at: i64,
    /// App ID or window class of the app focused when it was copied.
    pub source: Option<String>,
}

/// Why a capture was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skipped {
    Empty,
    Excluded,
    TooLarge(String),
    Failed(String),
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("nothing to record"),
            Self::Excluded => f.write_str("matches addons.clipboard.exclude_patterns"),
            Self::TooLarge(reason) => write!(f, "too large: {reason}"),
            Self::Failed(error) => f.write_str(error),
        }
    }
}

/// `$XDG_DATA_HOME/raffi/clipboard.db`. Disabled in tests so they never
/// touch the real history.
pub(crate) fn default_path() -> Result<PathBuf, String> {
    if cfg!(test) {
        return Err("the clipboard history database is disabled in tests".into());
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("cannot find the data directory: HOME is not set")?;
    Ok(base.join("raffi").join("clipboard.db"))
}

pub(crate) struct Store {
    conn: Connection,
}

fn db_error(error: rusqlite::Error) -> String {
    format!("clipboard history: {error}")
}

impl Store {
    pub(crate) fn open_default() -> Result<Self, String> {
        Self::open(&default_path()?)
    }

    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            if !dir.exists() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
            }
        }
        let conn = Connection::open(path).map_err(db_error)?;
        // SQLite gives the WAL and shared memory files the same mode.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        conn.busy_timeout(BUSY_TIMEOUT).map_err(db_error)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS items (
                 id INTEGER PRIMARY KEY,
                 hash TEXT NOT NULL UNIQUE,
                 seq INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 formats TEXT NOT NULL,
                 text TEXT NOT NULL,
                 text_len INTEGER NOT NULL,
                 trim_len INTEGER NOT NULL,
                 is_url INTEGER NOT NULL,
                 copied_at INTEGER NOT NULL,
                 source TEXT,
                 size INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS items_seq ON items (seq);
             CREATE INDEX IF NOT EXISTS items_kind ON items (kind, seq);
             CREATE TABLE IF NOT EXISTS formats (
                 item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
                 mime TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (item_id, mime)
             );",
        )
        .map_err(db_error)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_error)?;
        Ok(Self { conn })
    }

    /// Record `capture`, or move the identical entry to the top. Returns the
    /// entry's id.
    #[cfg(test)]
    pub(crate) fn insert(&mut self, capture: Capture, limits: &Limits) -> Result<String, Skipped> {
        self.insert_filtered(capture, limits, &[])
    }

    /// Like `insert`, but rejects the capture when its normalized searchable
    /// text matches one of `exclude`. Nothing is written in that case.
    pub(crate) fn insert_filtered(
        &mut self,
        capture: Capture,
        limits: &Limits,
        exclude: &[regex::Regex],
    ) -> Result<String, Skipped> {
        let mut formats = capture.formats;
        formats.retain(|(_, data)| !data.is_empty());
        if formats.is_empty() {
            return Err(Skipped::Empty);
        }
        for (mime, data) in &formats {
            let limit = if mime.starts_with("image/") {
                limits.max_image_size
            } else {
                MAX_TEXT_BYTES as u64
            };
            if data.len() as u64 > limit {
                return Err(Skipped::TooLarge(format!(
                    "{mime} is {} bytes, over the {limit} byte limit",
                    data.len()
                )));
            }
        }
        formats.sort_by(|a, b| a.0.cmp(&b.0));
        formats.dedup_by(|a, b| a.0 == b.0);

        let hash = fingerprint(&formats);
        let mimes: Vec<&str> = formats.iter().map(|(mime, _)| mime.as_str()).collect();
        let kind = kind_of(&mimes);
        let text = text_of(&formats);
        if is_excluded(&text, exclude) {
            return Err(Skipped::Excluded);
        }
        let size: usize = formats.iter().map(|(_, data)| data.len()).sum();
        let source = capture
            .source
            .map(|app| app.trim().to_string())
            .filter(|app| !app.is_empty());

        let result = (|| -> Result<(), String> {
            // Immediate, so the recorder and the selecting connection never
            // compute the same next `seq`.
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let updated = tx
                .execute(
                    &format!(
                        "UPDATE items SET seq = {NEXT_SEQ}, copied_at = ?1,
                             source = COALESCE(?2, source) WHERE hash = ?3"
                    ),
                    params![capture.copied_at, source, hash],
                )
                .map_err(db_error)?;
            if updated == 0 {
                tx.execute(
                    &format!(
                        "INSERT INTO items (hash, seq, kind, formats, text, text_len, trim_len,
                             is_url, copied_at, source, size)
                         VALUES (?1, {NEXT_SEQ}, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
                    ),
                    params![
                        hash,
                        kind_name(kind),
                        mimes.join("\n"),
                        text,
                        text.encode_utf16().count() as i64,
                        text.trim().encode_utf16().count() as i64,
                        is_url(&text),
                        capture.copied_at,
                        source,
                        size as i64,
                    ],
                )
                .map_err(db_error)?;
                let id = tx.last_insert_rowid();
                for (mime, data) in &formats {
                    tx.execute(
                        "INSERT INTO formats (item_id, mime, data) VALUES (?1, ?2, ?3)",
                        params![id, mime, data],
                    )
                    .map_err(db_error)?;
                }
            }
            tx.commit().map_err(db_error)?;
            self.prune(limits)
        })();
        result.map_err(Skipped::Failed).map(|()| hash)
    }

    /// Drop the oldest entries beyond `max_items`, then until the history
    /// fits in `max_size`. The newest entry is always kept.
    fn prune(&self, limits: &Limits) -> Result<(), String> {
        let mut statement = self
            .conn
            .prepare("SELECT id, size FROM items ORDER BY seq DESC")
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .map_err(db_error)?;
        let mut total = 0u64;
        let mut doomed = Vec::new();
        for (index, row) in rows.enumerate() {
            let (id, size) = row.map_err(db_error)?;
            total = total.saturating_add(size.max(0) as u64);
            if index > 0 && (index >= limits.max_items || total > limits.max_size) {
                doomed.push(id);
            }
        }
        for id in doomed {
            self.conn
                .execute("DELETE FROM items WHERE id = ?1", [id])
                .map_err(db_error)?;
        }
        Ok(())
    }

    /// Move the entry `hash` to the top, as when it is copied again.
    pub(crate) fn bump(&self, hash: &str) -> Result<(), String> {
        self.conn
            .execute(
                &format!("UPDATE items SET seq = {NEXT_SEQ} WHERE hash = ?1"),
                [hash],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Every matching entry, newest first, as one complete page. Matching
    /// and ranking follow the CopyQ listing script.
    pub(crate) fn list(
        &self,
        query: &str,
        only_kind: &str,
        min_chars: usize,
        exclude: &[regex::Regex],
    ) -> Result<Page, String> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let phrase = terms.join(" ");
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .map_err(db_error)?;
        let head: String = self
            .conn
            .query_row(
                "SELECT hash FROM items ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or_default();

        let filter = match only_kind {
            "image" => "kind = 'image'",
            "url" => "kind IN ('text', 'html') AND is_url = 1",
            _ => "1",
        };
        // Without a query only the start of each text is needed, unless
        // exclusions must see the complete text.
        let text = if terms.is_empty() && exclude.is_empty() {
            format!("substr(text, 1, {EXCERPT_CHARS})")
        } else {
            "text".to_string()
        };
        let sql = format!(
            "SELECT hash, kind, formats, {text}, text_len, copied_at, source FROM items
             WHERE {filter} AND NOT (kind IN ('text', 'html') AND trim_len < ?1)
             ORDER BY seq DESC"
        );
        let mut statement = self.conn.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map([min_chars as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(db_error)?;

        let mut items = Vec::new();
        for (row, entry) in rows.enumerate() {
            let (hash, kind, formats, text, text_len, copied_at, source) =
                entry.map_err(db_error)?;
            if is_excluded(&text, exclude) {
                continue;
            }
            let Some(tier) = tier_of(&text, &terms, &phrase) else {
                continue;
            };
            items.push(ClipItem {
                row,
                id: hash,
                formats: formats.split('\n').map(str::to_string).collect(),
                kind: parse_kind(&kind),
                text_len: text_len.max(0) as usize,
                excerpt: text.chars().take(EXCERPT_CHARS).collect(),
                tier,
                copied: local_time(copied_at),
                source,
                oversized: false,
            });
        }
        let total = total.max(0) as usize;
        Ok(Page {
            tab: TAB.to_string(),
            total,
            head,
            next: total.max(items.len()),
            stable: true,
            items,
        })
    }

    fn item_id(&self, hash: &str) -> Result<i64, String> {
        self.conn
            .query_row("SELECT id FROM items WHERE hash = ?1", [hash], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| "item is no longer in the clipboard history".to_string())
    }

    pub(crate) fn preview(&self, hash: &str) -> Result<ClipPreview, String> {
        let (id, kind, text, text_len): (i64, String, String, i64) = self
            .conn
            .query_row(
                &format!(
                    "SELECT id, kind, substr(text, 1, {PREVIEW_TEXT_BYTES}), text_len
                     FROM items WHERE hash = ?1"
                ),
                [hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| "item is no longer in the clipboard history".to_string())?;
        let kind = parse_kind(&kind);
        let mut statement = self
            .conn
            .prepare("SELECT mime, length(data) FROM formats WHERE item_id = ?1 ORDER BY mime")
            .map_err(db_error)?;
        let formats = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?.max(0) as usize,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;

        let image = preview_image_format(&formats).map(|(mime, size)| {
            if size > MAX_IMAGE_BYTES {
                return Err(format!(
                    "Image is too large to preview ({}, limit {})",
                    super::preview::format_byte_size(size as u64),
                    super::preview::format_byte_size(MAX_IMAGE_BYTES as u64)
                ));
            }
            let data: Vec<u8> = self
                .conn
                .query_row(
                    "SELECT data FROM formats WHERE item_id = ?1 AND mime = ?2",
                    params![id, mime],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            decode_image(&mime, &data)
        });

        let (text, text_truncated) = match kind {
            ItemKind::Image => (String::new(), false),
            _ => bounded_text(&text, text_len.max(0) as usize),
        };
        let files = if kind == ItemKind::Files {
            inspect_files(&text)
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

    /// Every stored format of the entry `hash`, to put it back on the
    /// clipboard.
    pub(crate) fn formats(&self, hash: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
        let id = self.item_id(hash)?;
        let mut statement = self
            .conn
            .prepare("SELECT mime, data FROM formats WHERE item_id = ?1 ORDER BY mime")
            .map_err(db_error)?;
        let formats = statement
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        Ok(formats)
    }

    pub(crate) fn delete(&self, hash: &str) -> Result<(), String> {
        let removed = self
            .conn
            .execute("DELETE FROM items WHERE hash = ?1", [hash])
            .map_err(db_error)?;
        if removed == 0 {
            return Err("item is no longer in the clipboard history".into());
        }
        Ok(())
    }
}

/// The image format shown in the preview: PNG when present, then any raster
/// format, SVG last since it cannot be previewed.
fn preview_image_format(formats: &[(String, usize)]) -> Option<(String, usize)> {
    let images = || {
        formats
            .iter()
            .filter(|(mime, _)| mime.starts_with("image/"))
    };
    images()
        .find(|(mime, _)| mime == "image/png")
        .or_else(|| images().find(|(mime, _)| mime != "image/svg+xml"))
        .or_else(|| images().next())
        .cloned()
}

fn local_time(secs: i64) -> Option<NaiveDateTime> {
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|time| time.naive_local())
}

/// 128-bit FNV-1a over every format, length-prefixed so that boundaries
/// between formats cannot collide.
fn fingerprint(formats: &[(String, Vec<u8>)]) -> String {
    let mut hash = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d_u128;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u128::from(*byte);
            hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
        }
    };
    for (mime, data) in formats {
        feed(&(mime.len() as u64).to_le_bytes());
        feed(mime.as_bytes());
        feed(&(data.len() as u64).to_le_bytes());
        feed(data);
    }
    format!("{hash:032x}")
}

fn kind_of(mimes: &[&str]) -> ItemKind {
    if mimes.iter().any(|mime| mime.starts_with("image/")) {
        ItemKind::Image
    } else if mimes.contains(&"text/uri-list") {
        ItemKind::Files
    } else if mimes.contains(&"text/plain") {
        ItemKind::Text
    } else if mimes.contains(&"text/html") {
        ItemKind::Html
    } else {
        ItemKind::Other
    }
}

fn kind_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Text => "text",
        ItemKind::Image => "image",
        ItemKind::Files => "files",
        ItemKind::Html => "html",
        ItemKind::Other => "other",
    }
}

fn parse_kind(name: &str) -> ItemKind {
    match name {
        "text" => ItemKind::Text,
        "image" => ItemKind::Image,
        "files" => ItemKind::Files,
        "html" => ItemKind::Html,
        _ => ItemKind::Other,
    }
}

static STYLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?is)<(style|script)[^>]*>[^<]*</(style|script)>").expect("valid regex")
});
static TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<[^>]*>").expect("valid regex"));

/// Whether `text` matches any exclusion pattern. Entries without text are
/// never excluded.
pub(crate) fn is_excluded(text: &str, patterns: &[regex::Regex]) -> bool {
    !text.is_empty() && patterns.iter().any(|pattern| pattern.is_match(text))
}

/// Searchable text: plain text, else the file list, else the HTML with its
/// tags removed.
pub(crate) fn text_of(formats: &[(String, Vec<u8>)]) -> String {
    let format = |name: &str| {
        formats
            .iter()
            .find(|(mime, _)| mime == name)
            .map(|(_, data)| String::from_utf8_lossy(data).into_owned())
    };
    if let Some(text) = format("text/plain").or_else(|| format("text/uri-list")) {
        return text;
    }
    let Some(html) = format("text/html") else {
        return String::new();
    };
    let html = STYLE.replace_all(&html, " ");
    TAG.replace_all(&html, " ")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Whether the whole text is one URL: a scheme with `://`, or one of the
/// schemes written without it, then no whitespace, control character or
/// angle bracket.
fn is_url(text: &str) -> bool {
    let text = text.trim();
    let scheme_end = text
        .find("://")
        .filter(|&at| {
            let scheme = &text[..at];
            scheme
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        })
        .map(|at| at + 3)
        .or_else(|| {
            let (scheme, _) = text.split_once(':')?;
            ["mailto", "tel", "magnet", "urn", "sms", "geo"]
                .iter()
                .any(|known| scheme.eq_ignore_ascii_case(known))
                .then_some(scheme.len() + 1)
        });
    let Some(scheme_end) = scheme_end else {
        return false;
    };
    scheme_end < text.len()
        && !text
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '<' || c == '>')
}

/// 0 when the text starts with the query, 1 when it contains it, 2 when it
/// contains every word, `None` otherwise.
fn tier_of(text: &str, terms: &[String], phrase: &str) -> Option<u8> {
    if terms.is_empty() {
        return Some(0);
    }
    let lower = text.to_lowercase();
    if !terms.iter().all(|term| lower.contains(term.as_str())) {
        return None;
    }
    if !lower.contains(phrase) {
        return Some(2);
    }
    Some(if lower.trim().starts_with(phrase) {
        0
    } else {
        1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("raffi-clipstore-{name}-{unique}"));
        let store = Store::open(&dir.join("clipboard.db")).unwrap();
        (store, dir)
    }

    const LIMITS: Limits = Limits {
        max_items: 100,
        max_size: 1 << 30,
        max_image_size: 1 << 20,
    };

    fn text(value: &str, at: i64) -> Capture {
        Capture {
            formats: vec![("text/plain".into(), value.as_bytes().to_vec())],
            copied_at: at,
            source: Some("kitty".into()),
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn test_concurrent_connections_never_share_a_position() {
        let (mut store, dir) = temp_store("concurrent");
        let hash = store.insert(text("selected entry", 1), &LIMITS).unwrap();
        let path = dir.join("clipboard.db");
        let selector = std::thread::spawn(move || {
            let other = Store::open(&path).unwrap();
            for _ in 0..200 {
                other.bump(&hash).unwrap();
            }
        });
        for index in 0..200 {
            store
                .insert(text(&format!("copy number {index}"), 2), &LIMITS)
                .unwrap();
        }
        selector.join().unwrap();
        let (rows, positions): (i64, i64) = store
            .conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT seq) FROM items",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, positions);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn titles(page: &Page) -> Vec<String> {
        page.items.iter().map(ClipItem::title).collect()
    }

    #[test]
    fn test_insert_lists_newest_first_and_dedups_recopies() {
        let (mut store, dir) = temp_store("dedup");
        let first = store.insert(text("first entry", 100), &LIMITS).unwrap();
        store.insert(text("second entry", 200), &LIMITS).unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[]).unwrap()),
            ["second entry", "first entry"]
        );

        let again = store.insert(text("first entry", 300), &LIMITS).unwrap();
        assert_eq!(again, first);
        let page = store.list("", "", 0, &[]).unwrap();
        assert_eq!(titles(&page), ["first entry", "second entry"]);
        assert_eq!(page.total, 2);
        assert_eq!(page.head, first);
        assert_eq!(page.items[0].copied, local_time(300));
        assert_eq!(page.items[0].source.as_deref(), Some("kitty"));

        store.bump(&page.items[1].id).unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[]).unwrap()),
            ["second entry", "first entry"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_prune_by_count_and_size() {
        let (mut store, dir) = temp_store("prune");
        let limits = Limits {
            max_items: 3,
            max_size: 25,
            ..LIMITS
        };
        for (index, value) in ["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc", "dddddddddd"]
            .into_iter()
            .enumerate()
        {
            store.insert(text(value, index as i64), &limits).unwrap();
        }
        // Three fit by count, but only two fit in 25 bytes.
        assert_eq!(
            titles(&store.list("", "", 0, &[]).unwrap()),
            ["dddddddddd", "cccccccccc"]
        );

        let tiny = Limits {
            max_size: 1,
            ..limits
        };
        store.insert(text("eeeeeeeeee", 9), &tiny).unwrap();
        assert_eq!(titles(&store.list("", "", 0, &[]).unwrap()), ["eeeeeeeeee"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_oversized_images_and_empty_captures_are_skipped() {
        let (mut store, dir) = temp_store("skip");
        let limits = Limits {
            max_image_size: 10,
            ..LIMITS
        };
        let capture = Capture {
            formats: vec![("image/png".into(), png(4, 4))],
            copied_at: 1,
            source: None,
        };
        assert!(matches!(
            store.insert(capture, &limits),
            Err(Skipped::TooLarge(_))
        ));
        let empty = Capture {
            formats: vec![("text/plain".into(), Vec::new())],
            copied_at: 1,
            source: None,
        };
        assert_eq!(store.insert(empty, &limits), Err(Skipped::Empty));
        assert_eq!(store.list("", "", 0, &[]).unwrap().total, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_list_search_tiers_and_filters() {
        let (mut store, dir) = temp_store("search");
        for (index, value) in [
            "hello world",
            "say hello world",
            "world says hello",
            "unrelated",
            "https://example.com/page",
            "ok",
        ]
        .into_iter()
        .enumerate()
        {
            store.insert(text(value, index as i64), &LIMITS).unwrap();
        }
        let image = Capture {
            formats: vec![
                ("image/png".into(), png(3, 2)),
                ("text/html".into(), b"<img src=\"x.png\">".to_vec()),
            ],
            copied_at: 10,
            source: Some("firefox".into()),
        };
        store.insert(image, &LIMITS).unwrap();

        let page = store.list("Hello  World", "", 0, &[]).unwrap();
        let ranked: Vec<(String, u8)> = page
            .items
            .iter()
            .map(|item| (item.title(), item.tier))
            .collect();
        assert_eq!(
            ranked,
            [
                ("world says hello".to_string(), 2),
                ("say hello world".to_string(), 1),
                ("hello world".to_string(), 0),
            ]
        );

        let images = store.list("", "image", 0, &[]).unwrap();
        assert_eq!(images.items.len(), 1);
        assert_eq!(images.items[0].kind, ItemKind::Image);
        assert_eq!(images.items[0].title(), "Image (PNG)");

        let urls = store.list("", "url", 0, &[]).unwrap();
        assert_eq!(titles(&urls), ["https://example.com/page"]);

        let all = store.list("", "", 3, &[]).unwrap();
        assert!(!titles(&all).contains(&"ok".to_string()));
        assert_eq!(all.items.len(), 6);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_insert_filtered_checks_text_after_normalization() {
        let (mut store, dir) = temp_store("normalized-exclude");
        let patterns = [regex::Regex::new("(?i)password").unwrap()];
        let capture = Capture {
            formats: vec![
                ("text/plain".into(), Vec::new()),
                ("text/html".into(), b"<p>Password = x</p>".to_vec()),
            ],
            copied_at: 1,
            source: None,
        };

        assert_eq!(
            store.insert_filtered(capture, &LIMITS, &patterns),
            Err(Skipped::Excluded)
        );
        assert!(store.list("", "", 0, &[]).unwrap().items.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_list_hides_entries_matching_exclusions() {
        let (mut store, dir) = temp_store("exclude");
        store.insert(text("secret: hunter2", 1), &LIMITS).unwrap();
        store
            .insert(text("line one\nPASSWORD=abc", 2), &LIMITS)
            .unwrap();
        store
            .insert(
                Capture {
                    formats: vec![("text/html".into(), b"<p>Password = x</p>".to_vec())],
                    copied_at: 3,
                    source: None,
                },
                &LIMITS,
            )
            .unwrap();
        let long = format!("{}needle", "a".repeat(EXCERPT_CHARS + 50));
        store.insert(text(&long, 4), &LIMITS).unwrap();
        store.insert(text("keep this entry", 5), &LIMITS).unwrap();

        let exclude = [
            regex::Regex::new("^secret:").unwrap(),
            regex::Regex::new("(?i)password\\s*=").unwrap(),
            regex::Regex::new("needle").unwrap(),
        ];
        let page = store.list("", "", 0, &exclude).unwrap();
        assert_eq!(titles(&page), ["keep this entry"]);
        assert_eq!(page.total, 5);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_is_excluded_ignores_empty_text() {
        let patterns = [regex::Regex::new("").unwrap()];
        assert!(!is_excluded("", &patterns));
        assert!(is_excluded("anything", &patterns));
        assert!(!is_excluded("anything", &[]));
    }

    #[test]
    fn test_preview_text_image_and_files() {
        let (mut store, dir) = temp_store("preview");
        let text_id = store
            .insert(text("line one\nline two", 1), &LIMITS)
            .unwrap();
        let preview = store.preview(&text_id).unwrap();
        assert_eq!(preview.kind, ItemKind::Text);
        assert_eq!(preview.text, "line one\nline two");
        assert_eq!(preview.formats, [("text/plain".to_string(), 17)]);
        assert!(preview.image.is_none());

        let bytes = png(5, 3);
        let image_id = store
            .insert(
                Capture {
                    formats: vec![
                        ("image/svg+xml".into(), b"<svg/>".to_vec()),
                        ("image/png".into(), bytes.clone()),
                    ],
                    copied_at: 2,
                    source: None,
                },
                &LIMITS,
            )
            .unwrap();
        let preview = store.preview(&image_id).unwrap();
        assert_eq!(preview.kind, ItemKind::Image);
        let image = preview.image.unwrap().unwrap();
        assert_eq!(
            (image.mime.as_str(), image.width, image.height),
            ("image/png", 5, 3)
        );
        assert_eq!(
            store.formats(&image_id).unwrap(),
            [
                ("image/png".to_string(), bytes),
                ("image/svg+xml".to_string(), b"<svg/>".to_vec()),
            ]
        );

        let files_id = store
            .insert(
                Capture {
                    formats: vec![(
                        "text/uri-list".into(),
                        b"file:///definitely/missing%20file\r\n".to_vec(),
                    )],
                    copied_at: 3,
                    source: None,
                },
                &LIMITS,
            )
            .unwrap();
        let preview = store.preview(&files_id).unwrap();
        assert_eq!(preview.kind, ItemKind::Files);
        assert_eq!(preview.files[0].name, "/definitely/missing file");
        assert_eq!(preview.files[0].missing, Some(true));

        store.delete(&text_id).unwrap();
        assert!(store.preview(&text_id).is_err());
        assert!(store.delete(&text_id).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_html_text_and_url_detection() {
        let formats = vec![(
            "text/html".to_string(),
            b"<style>p{}</style><p>Tom &amp; Jerry&nbsp;&lt;3</p>".to_vec(),
        )];
        assert_eq!(
            text_of(&formats).split_whitespace().collect::<Vec<_>>(),
            ["Tom", "&", "Jerry", "<3"]
        );
        assert!(is_url(" https://example.com/a?b=c "));
        assert!(is_url("mailto:me@example.com"));
        assert!(!is_url("https://"));
        assert!(!is_url("https://example.com and more"));
        assert!(!is_url("not a url"));
        assert!(!is_url("1http://example.com"));
    }
}
