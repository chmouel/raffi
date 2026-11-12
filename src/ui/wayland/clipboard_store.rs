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
//!
//! Entries captured as temporary keep an absolute deadline and leave the
//! history, with their formats, once it passes. The deadline is written with
//! the entry and never follows a configuration change; expiry only touches
//! the history, not the selection on the clipboard.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::clipboard::{
    bounded_text, decode_image, image_dimensions, inspect_files, ClipItem, ClipPreview, ItemKind,
    Page, EXCERPT_CHARS, MAX_IMAGE_BYTES, PREVIEW_TEXT_BYTES,
};
use super::clipboard_policy::{Disposition, Policy, PASSWORD_HINT};

/// Tab name reported in pages; the raffi history has a single list.
pub(crate) const TAB: &str = "raffi";
/// Text formats larger than this are not recorded.
pub(crate) const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// Longest MIME name a format may carry, its parameters included.
pub(crate) const MAX_MIME_BYTES: usize = 256;
/// Start of a stored image read to find its pixel size without loading it.
const IMAGE_HEADER_BYTES: usize = 64 * 1024;
/// Next history position, computed inside the statement that assigns it so
/// concurrent connections cannot hand out the same one.
const NEXT_SEQ: &str = "(SELECT COALESCE(MAX(seq), 0) + 1 FROM items)";
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const SCHEMA_VERSION: i64 = 3;
/// SQLite virtual machine steps between two checks of a listing's cancel
/// flag.
const CANCEL_CHECK_STEPS: i32 = 4096;
/// How often the sweeper looks for entries that reached their deadline.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// SQL predicate on `items`, with `now` bound as the first parameter: the
/// entry is still in the history at that second.
const ALIVE: &str = "(expires_at IS NULL OR expires_at > ?1)";
/// SQL predicate on `items`, with `now` bound as the first parameter: the
/// entry reached its deadline at that second.
const EXPIRED: &str = "(expires_at IS NOT NULL AND expires_at <= ?1)";

/// Message of a lookup that finds the entry gone, which the command line
/// reports as `not_found` rather than as a database failure.
pub(crate) const GONE: &str = "item is no longer in the clipboard history";

/// Whether `message` is [`GONE`].
pub(crate) fn is_gone(message: &str) -> bool {
    message == GONE
}

/// Stored for the password-manager marker when the source offers it with no
/// data, so that the marker survives the empty-format filter and the entry
/// is served with it again.
pub(crate) const PASSWORD_MARKER: &[u8] = b"secret";

/// Reports that the listing it was handed to has been replaced by a newer
/// one and can stop.
pub(crate) type Cancel = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// Unix time, injected into every operation that depends on the clock.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

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
    /// The source marked the selection as a secret with
    /// `x-kde-passwordManagerHint`.
    pub password_marked: bool,
}

/// Why a capture was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skipped {
    Empty,
    Excluded,
    /// The entry reached its deadline before it could be recorded.
    Expired,
    TooLarge(String),
    Failed(String),
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("nothing to record"),
            Self::Excluded => f.write_str("excluded by addons.clipboard"),
            Self::Expired => f.write_str("already expired"),
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

/// Whether this installation has a history database.
pub(crate) fn exists() -> bool {
    default_path().is_ok_and(|path| path.exists())
}

/// Bytes the history database and its write-ahead log take on disk.
pub(crate) fn disk_usage() -> u64 {
    let Ok(path) = default_path() else {
        return 0;
    };
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    [path, PathBuf::from(wal)]
        .iter()
        .map(|path| fs::metadata(path).map_or(0, |meta| meta.len()))
        .sum()
}

/// The pixel size of a stored image, with its format and stored size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageMeta {
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

/// What the history holds, for `raffi clipboard status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Every stored entry, expired ones included.
    pub total: usize,
    /// Entries still in the history at the time of the call.
    pub alive: usize,
    /// Entries that leave the history when their deadline passes.
    pub temporary: usize,
    /// Bytes of every stored format of the live entries.
    pub bytes: u64,
}

/// SQL predicate on `items` for a kind filter, `None` for an unknown one.
/// Listing and deletion both go through it, so a kind means the same thing in
/// both and no unknown value ever reaches a `DELETE`.
pub(crate) fn kind_predicate(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "text" => "kind = 'text'",
        "html" => "kind = 'html'",
        "image" => "kind = 'image'",
        "files" => "kind = 'files'",
        "url" => "kind IN ('text', 'html') AND is_url = 1",
        "other" => "kind = 'other'",
        _ => return None,
    })
}

pub(crate) struct Store {
    conn: Connection,
    /// The write-ahead log next to the database.
    wal: PathBuf,
}

fn db_error(error: rusqlite::Error) -> String {
    format!("clipboard history: {error}")
}

/// Create the schema, or upgrade a database written by an older Raffi.
/// Version 2 added `items.expires_at`; the rows recorded before it have no
/// deadline and are kept as they are. Version 3 added `items.search`, filled
/// in for the existing rows.
///
/// The daemon's threads open the database at the same time, so the checks
/// and changes run in one write transaction: whoever comes second waits and
/// then finds the schema current.
fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(db_error)?;
    if schema_version(conn)? >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    if schema_version(&tx)? >= SCHEMA_VERSION {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS items (
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
             size INTEGER NOT NULL,
             expires_at INTEGER,
             search TEXT NOT NULL DEFAULT ''
         );
         CREATE TABLE IF NOT EXISTS formats (
             item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
             mime TEXT NOT NULL,
             data BLOB NOT NULL,
             PRIMARY KEY (item_id, mime)
         );",
    )
    .map_err(db_error)?;
    if !has_column(&tx, "items", "expires_at")? {
        tx.execute_batch("ALTER TABLE items ADD COLUMN expires_at INTEGER")
            .map_err(db_error)?;
    }
    if !has_column(&tx, "items", "search")? {
        add_search_column(&tx)?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS items_seq ON items (seq);
         CREATE INDEX IF NOT EXISTS items_kind ON items (kind, seq);
         CREATE INDEX IF NOT EXISTS items_expires_at ON items (expires_at)
             WHERE expires_at IS NOT NULL;",
    )
    .map_err(db_error)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

fn schema_version(conn: &Connection) -> Result<i64, String> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)
}

/// Add `items.search` and fill it from the text of every existing entry,
/// inside the migration's transaction so a crash cannot leave entries
/// unsearchable.
fn add_search_column(tx: &Connection) -> Result<(), String> {
    tx.execute_batch("ALTER TABLE items ADD COLUMN search TEXT NOT NULL DEFAULT ''")
        .map_err(db_error)?;
    let rows: Vec<(i64, String)> = {
        let mut statement = tx
            .prepare("SELECT id, text FROM items WHERE text != ''")
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_error)?;
        rows.collect::<Result<_, _>>().map_err(db_error)?
    };
    for (id, text) in rows {
        tx.execute(
            "UPDATE items SET search = ?1 WHERE id = ?2",
            params![searchable(&text), id],
        )
        .map_err(db_error)?;
    }
    Ok(())
}

/// What a query is matched against: the text trimmed and lowercased once at
/// capture, so a search does not lowercase the whole history again.
fn searchable(text: &str) -> String {
    text.trim().to_lowercase()
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let count: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            [column],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    Ok(count > 0)
}

/// Remove the entries that reached their deadline at `now`, leaving the
/// failures to the sweeper's next tick.
fn sweep(store: &mut Store, now: i64) -> Result<usize, String> {
    let removed = store.expire(now)?;
    if removed > 0 {
        crate::debug_log!("clipboard: removed {removed} expired entries");
    }
    // A delete whose log could not be emptied, because a listing was still
    // reading, is retried here until it succeeds.
    if store.wal_has_frames() {
        store.truncate_wal();
    }
    Ok(removed)
}

/// Remove expired entries once at startup, then every second, on a thread of
/// its own: cleanup must not wait for a copy or for the compositor.
pub(crate) fn start_sweeper() {
    std::thread::Builder::new()
        .name("raffi-clipboard-sweep".into())
        .spawn(|| {
            let mut store: Option<Store> = None;
            loop {
                let result = match store.as_mut() {
                    Some(store) => sweep(store, now_secs()).map(|_| ()),
                    None => Store::open_default().map(|opened| store = Some(opened)),
                };
                if let Err(error) = result {
                    eprintln!("raffi: clipboard history cleanup failed: {error}");
                    // Reopening on the next tick retries a locked or damaged
                    // database without stopping the sweeper.
                    store = None;
                }
                std::thread::sleep(SWEEP_INTERVAL);
            }
        })
        .ok();
}

impl Store {
    pub(crate) fn open_default() -> Result<Self, String> {
        Self::open(&default_path()?)
    }

    /// Open the history without creating a directory or a file, so a command
    /// that only reads or reports never leaves a database behind.
    pub(crate) fn open_existing() -> Result<Option<Self>, String> {
        Self::open_existing_at(&default_path()?)
    }

    /// Like `open_existing`, at an explicit path. The open itself refuses to
    /// create the file, so a database removed since the existence check
    /// cannot come back as an empty one.
    pub(crate) fn open_existing_at(path: &Path) -> Result<Option<Self>, String> {
        if !path.exists() {
            return Ok(None);
        }
        Self::open_read_write(path).map(Some)
    }

    fn open_read_write(path: &Path) -> Result<Self, String> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
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
        Self::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
    }

    fn open_with_flags(path: &Path, flags: OpenFlags) -> Result<Self, String> {
        let conn = Connection::open_with_flags(path, flags).map_err(db_error)?;
        // SQLite gives the WAL and shared memory files the same mode.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        conn.busy_timeout(BUSY_TIMEOUT).map_err(db_error)?;
        // Deleted entries, temporary and password-manager ones above all,
        // are overwritten instead of lingering in free pages of the file.
        conn.pragma_update(None, "secure_delete", "ON")
            .map_err(db_error)?;
        migrate(&conn)?;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        Ok(Self {
            conn,
            wal: PathBuf::from(wal),
        })
    }

    /// Record `capture`, or move the identical entry to the top. Returns the
    /// entry's id.
    #[cfg(test)]
    pub(crate) fn insert(
        &mut self,
        capture: Capture,
        limits: &Limits,
        now: i64,
    ) -> Result<String, Skipped> {
        self.insert_filtered(capture, limits, &Policy::default(), now)
    }

    /// Like `insert`, but drops the capture when `policy` skips it. A
    /// temporary entry gets the deadline `capture.copied_at` plus the policy
    /// timeout, so copying the same thing again moves it up with a new
    /// deadline from the current policy.
    pub(crate) fn insert_filtered(
        &mut self,
        capture: Capture,
        limits: &Limits,
        policy: &Policy,
        now: i64,
    ) -> Result<String, Skipped> {
        self.record(capture, limits, policy, now, false)
    }

    /// Like `insert_filtered`, but keeps the entry temporary even when no
    /// temporary pattern matches: `raffi clipboard set --temporary` asked for
    /// it. Exclusion patterns and password-manager rules still win.
    pub(crate) fn insert_temporary(
        &mut self,
        capture: Capture,
        limits: &Limits,
        policy: &Policy,
        now: i64,
    ) -> Result<String, Skipped> {
        self.record(capture, limits, policy, now, true)
    }

    fn record(
        &mut self,
        capture: Capture,
        limits: &Limits,
        policy: &Policy,
        now: i64,
        force_temporary: bool,
    ) -> Result<String, Skipped> {
        let password_marked = capture.password_marked;
        let mut formats = capture.formats;
        formats.retain(|(_, data)| !data.is_empty());
        if formats.is_empty() {
            return Err(Skipped::Empty);
        }
        for (mime, _) in &mut formats {
            *mime = canonical_mime(mime);
        }        for (mime, data) in &formats {
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

        // The marker follows the copy rather than its content: it is stored
        // as a format so the entry is served with it again, and a source may
        // offer it with no data at all.
        let marker = password_marked.then(|| {
            formats
                .iter()
                .find(|(mime, data)| mime == PASSWORD_HINT && !data.is_empty())
                .map(|(_, data)| data.clone())
                .unwrap_or_else(|| PASSWORD_MARKER.to_vec())
        });
        formats.retain(|(mime, _)| mime != PASSWORD_HINT);
        if let Some(marker) = &marker {
            formats.push((PASSWORD_HINT.to_string(), marker.clone()));
            formats.sort_by(|a, b| a.0.cmp(&b.0));
        }

        // Identical content keeps one entry, whether or not it was copied
        // with the marker: the marker is left out of the fingerprint so the
        // latest copy decides the deadline and the marker of that entry.
        let hash = fingerprint(
            formats
                .iter()
                .filter(|(mime, _)| mime != PASSWORD_HINT)
                .map(|(mime, data)| (mime.as_str(), data.as_slice())),
        );
        // The marker is not content: it is left out of the kind the entry is
        // listed with, of its searchable text and of its formats.
        let mimes: Vec<&str> = formats
            .iter()
            .filter(|(mime, _)| mime != PASSWORD_HINT)
            .map(|(mime, _)| mime.as_str())
            .collect();
        let kind = kind_of(&mimes);
        let text = text_of(&formats);
        let deadline = deadline_for(
            policy.disposition(&text, password_marked),
            password_marked,
            capture.copied_at,
            policy.temporary_timeout,
        )?;
        let expires_at = deadline.or_else(|| {
            force_temporary.then(|| capture.copied_at.saturating_add(policy.temporary_timeout))
        });
        if expires_at.is_some_and(|deadline| deadline <= now) {
            return Err(Skipped::Expired);
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
                        "UPDATE items SET seq = {NEXT_SEQ}, copied_at = ?1, expires_at = ?2,
                             source = COALESCE(?3, source), formats = ?4, size = ?5
                         WHERE hash = ?6"
                    ),
                    params![
                        capture.copied_at,
                        expires_at,
                        source,
                        mimes.join("\n"),
                        size as i64,
                        hash
                    ],
                )
                .map_err(db_error)?;
            if updated > 0 {
                // The marker follows the latest copy: a marked copy makes the
                // entry temporary and served with the hint, an unmarked one
                // drops the hint again.
                tx.execute(
                    "DELETE FROM formats WHERE mime = ?1
                         AND item_id = (SELECT id FROM items WHERE hash = ?2)",
                    params![PASSWORD_HINT, hash],
                )
                .map_err(db_error)?;
                if let Some(marker) = &marker {
                    tx.execute(
                        "INSERT INTO formats (item_id, mime, data)
                             VALUES ((SELECT id FROM items WHERE hash = ?1), ?2, ?3)",
                        params![hash, PASSWORD_HINT, marker],
                    )
                    .map_err(db_error)?;
                }
            }
            if updated == 0 {
                tx.execute(
                    &format!(
                        "INSERT INTO items (hash, seq, kind, formats, text, text_len, trim_len,
                             is_url, copied_at, source, size, expires_at, search)
                         VALUES (?1, {NEXT_SEQ}, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
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
                        expires_at,
                        searchable(&text),
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
            self.prune(limits, now)
        })();
        result.map_err(Skipped::Failed).map(|()| hash)
    }

    /// Drop the oldest entries beyond `max_items`, then until the history
    /// fits in `max_size`. The newest entry that is still alive is always
    /// kept: expired entries are left to `expire` and count for nothing.
    fn prune(&self, limits: &Limits, now: i64) -> Result<(), String> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "SELECT id, size FROM items WHERE {ALIVE} ORDER BY seq DESC"
            ))
            .map_err(db_error)?;
        let rows = statement
            .query_map([now], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })
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

    /// The deadline of the entry `hash`, `None` when it stays in the history.
    pub(crate) fn deadline(&self, hash: &str) -> Result<Option<i64>, String> {
        self.conn
            .query_row(
                "SELECT expires_at FROM items WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| GONE.to_string())
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

    /// Every matching entry that is alive at `now`, newest first, as one
    /// complete page. Matching and ranking follow the CopyQ listing script.
    #[cfg(test)]
    pub(crate) fn list(
        &self,
        query: &str,
        only_kind: &str,
        min_chars: usize,
        exclude: &[regex::Regex],
        now: i64,
    ) -> Result<Page, String> {
        self.list_rows(query, only_kind, min_chars, exclude, now)
    }

    /// Like `list`, but gives up with an error as soon as `cancel` reports
    /// that a newer listing replaced this one, even in the middle of the
    /// SQLite scan.
    pub(crate) fn list_until(
        &mut self,
        query: &str,
        only_kind: &str,
        min_chars: usize,
        exclude: &[regex::Regex],
        now: i64,
        cancel: Option<Cancel>,
    ) -> Result<Page, String> {
        let Some(cancel) = cancel else {
            return self.list_rows(query, only_kind, min_chars, exclude, now);
        };
        if cancel() {
            return Err("clipboard listing replaced by a newer one".into());
        }
        self.conn
            .progress_handler(CANCEL_CHECK_STEPS, Some(move || cancel()))
            .map_err(db_error)?;
        let result = self.list_rows(query, only_kind, min_chars, exclude, now);
        self.conn
            .progress_handler(0, None::<fn() -> bool>)
            .map_err(db_error)?;
        result
    }

    fn list_rows(
        &self,
        query: &str,
        only_kind: &str,
        min_chars: usize,
        exclude: &[regex::Regex],
        now: i64,
    ) -> Result<Page, String> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let phrase = terms.join(" ");
        let total: i64 = self
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM items WHERE {ALIVE}"),
                [now],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let head: String = self
            .conn
            .query_row(
                &format!("SELECT hash FROM items WHERE {ALIVE} ORDER BY seq DESC LIMIT 1"),
                [now],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or_default();

        let filter = kind_predicate(only_kind).unwrap_or("1");
        // Words and phrase are matched in SQLite against the lowercased
        // `search` column, so only matching entries reach Rust. With
        // `?3` the phrase and `?4...` the words: tier 0 when the text starts
        // with the phrase, 1 when it contains it, 2 when it has every word.
        let mut values: Vec<rusqlite::types::Value> = vec![now.into(), (min_chars as i64).into()];
        let (tier, matching) = if terms.is_empty() {
            ("0".to_string(), String::new())
        } else {
            values.push(phrase.clone().into());
            let mut matching = String::new();
            for term in &terms {
                values.push(term.clone().into());
                matching.push_str(&format!(" AND instr(search, ?{}) > 0", values.len()));
            }
            (
                "CASE WHEN instr(search, ?3) = 1 THEN 0
                      WHEN instr(search, ?3) > 0 THEN 1 ELSE 2 END"
                    .to_string(),
                matching,
            )
        };
        // Only the start of each text is shown, so the complete text is
        // read only when exclusions must see it.
        let text = if exclude.is_empty() {
            format!("substr(text, 1, {EXCERPT_CHARS})")
        } else {
            "text".to_string()
        };
        let sql = format!(
            "SELECT hash, kind, formats, {text}, text_len, copied_at, source, expires_at, {tier}
             FROM items
             WHERE {ALIVE} AND {filter} AND NOT (kind IN ('text', 'html') AND trim_len < ?2)
                 {matching}
             ORDER BY seq DESC"
        );
        let mut statement = self.conn.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(db_error)?;

        let mut items = Vec::new();
        for (row, entry) in rows.enumerate() {
            let (hash, kind, formats, text, text_len, copied_at, source, expires_at, tier) =
                entry.map_err(db_error)?;
            if is_excluded(&text, exclude) {
                continue;
            }
            items.push(ClipItem {
                row,
                id: hash,
                formats: formats.split('\n').map(str::to_string).collect(),
                kind: parse_kind(&kind),
                text_len: text_len.max(0) as usize,
                excerpt: text.chars().take(EXCERPT_CHARS).collect(),
                tier: tier.clamp(0, 2) as u8,
                copied: Some(copied_at),
                source,
                oversized: false,
                expires_at,
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

    /// The id of the entry `hash`, as long as it is alive at `now`.
    fn item_id(&self, hash: &str, now: i64) -> Result<i64, String> {
        self.conn
            .query_row(
                &format!("SELECT id FROM items WHERE hash = ?2 AND {ALIVE}"),
                params![now, hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| GONE.to_string())
    }

    pub(crate) fn preview(&self, hash: &str, now: i64) -> Result<ClipPreview, String> {
        let (id, kind, text, text_len): (i64, String, String, i64) = self
            .conn
            .query_row(
                &format!(
                    "SELECT id, kind, substr(text, 1, {PREVIEW_TEXT_BYTES}), text_len
                     FROM items WHERE hash = ?2 AND {ALIVE}"
                ),
                params![now, hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| GONE.to_string())?;
        let kind = parse_kind(&kind);
        let mut statement = self
            .conn
            // The password-manager marker is not content, so the preview
            // does not list it.
            .prepare(
                "SELECT mime, length(data) FROM formats WHERE item_id = ?1 AND mime != ?2
                 ORDER BY mime",
            )
            .map_err(db_error)?;
        let formats = statement
            .query_map(params![id, PASSWORD_HINT], |row| {
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
    /// clipboard, as long as it is alive at `now`.
    pub(crate) fn formats(&self, hash: &str, now: i64) -> Result<Vec<(String, Vec<u8>)>, String> {
        let id = self.item_id(hash, now)?;
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

    /// Pixel size of the image format an entry holds, read from the start
    /// of the stored bytes rather than the whole picture.
    pub(crate) fn image_meta(&self, hash: &str, now: i64) -> Result<Option<ImageMeta>, String> {
        let id = self.item_id(hash, now)?;
        let row = self
            .conn
            .query_row(
                "SELECT mime, length(data), substr(data, 1, ?2) FROM formats
                 WHERE item_id = ?1 AND mime LIKE 'image/%'
                 ORDER BY CASE mime
                     WHEN 'image/png' THEN 0
                     WHEN 'image/svg+xml' THEN 3
                     ELSE 1
                 END, mime
                 LIMIT 1",
                params![id, IMAGE_HEADER_BYTES as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        Ok(row.and_then(|(mime, bytes, header)| {
            image_dimensions(&header).map(|(width, height)| ImageMeta {
                mime,
                width,
                height,
                bytes: bytes.max(0) as usize,
            })
        }))
    }

    /// Remove the entry `hash`. Reports whether the log was emptied, which
    /// it cannot be while another connection reads it.
    pub(crate) fn delete(&self, hash: &str) -> Result<bool, String> {
        let removed = self
            .conn
            .execute("DELETE FROM items WHERE hash = ?1", [hash])
            .map_err(db_error)?;
        if removed == 0 {
            return Err(GONE.to_string());
        }
        Ok(self.truncate_wal())
    }

    /// Delete every entry matching the SQL `filter`, hidden and expired ones
    /// included, and report how many entries went and whether the log was
    /// emptied.
    pub(crate) fn clear(&mut self, filter: &str) -> Result<(usize, bool), String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute(
            &format!("DELETE FROM formats WHERE item_id IN (SELECT id FROM items WHERE {filter})"),
            [],
        )
        .map_err(db_error)?;
        let removed = tx
            .execute(&format!("DELETE FROM items WHERE {filter}"), [])
            .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok((removed, self.truncate_wal()))
    }

    /// Entries matching the SQL `filter`: every stored one, and how many are
    /// still in the history at `now`.
    pub(crate) fn count(&self, filter: &str, now: i64) -> Result<(usize, usize), String> {
        let (total, alive): (i64, i64) = self
            .conn
            .query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM(CASE WHEN {ALIVE} THEN 1 ELSE 0 END), 0)
                     FROM items WHERE {filter}"
                ),
                [now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(db_error)?;
        Ok((total.max(0) as usize, alive.max(0) as usize))
    }

    /// Entries stored, entries alive at `now`, how many of those are
    /// temporary, and the bytes they take.
    pub(crate) fn counts(&self, now: i64) -> Result<Counts, String> {
        let (total, alive) = self.count("1", now)?;
        let (temporary, bytes): (i64, i64) = self
            .conn
            .query_row(
                &format!(
                    "SELECT COALESCE(SUM(CASE WHEN expires_at IS NOT NULL THEN 1 ELSE 0 END), 0),
                             COALESCE(SUM(size), 0)
                     FROM items WHERE {ALIVE}"
                ),
                [now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(db_error)?;
        Ok(Counts {
            total,
            alive,
            temporary: temporary.max(0) as usize,
            bytes: bytes.max(0) as u64,
        })
    }

    /// Delete the entries that reached their deadline at `now`, with the
    /// formats stored for them, and report how many entries went.
    pub(crate) fn expire(&mut self, now: i64) -> Result<usize, String> {
        // Checked first without a write lock, which most ticks do not need.
        let due: bool = self
            .conn
            .query_row(
                &format!("SELECT EXISTS (SELECT 1 FROM items WHERE {EXPIRED})"),
                [now],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if !due {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let doomed: Vec<i64> = {
            let mut statement = tx
                .prepare(&format!("SELECT id FROM items WHERE {EXPIRED}"))
                .map_err(db_error)?;
            let ids = statement
                .query_map([now], |row| row.get(0))
                .map_err(db_error)?
                .collect::<Result<Vec<i64>, _>>()
                .map_err(db_error)?;
            ids
        };
        for id in &doomed {
            tx.execute("DELETE FROM formats WHERE item_id = ?1", [id])
                .map_err(db_error)?;
        }
        let removed = tx
            .execute(&format!("DELETE FROM items WHERE {EXPIRED}"), [now])
            .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        self.truncate_wal();
        Ok(removed)
    }

    /// Copy the write-ahead log into the database and empty it, so the
    /// content of deleted entries does not stay in old log frames. Returns
    /// whether the log was emptied: it is not while another connection is
    /// reading, and the sweeper tries again later.
    pub(crate) fn truncate_wal(&self) -> bool {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                row.get::<_, i64>(0)
            })
            .is_ok_and(|busy| busy == 0)
    }

    fn wal_has_frames(&self) -> bool {
        fs::metadata(&self.wal).is_ok_and(|meta| meta.len() > 0)
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

/// 128-bit FNV-1a over every format, length-prefixed so that boundaries
/// between formats cannot collide.
fn fingerprint<'a>(formats: impl Iterator<Item = (&'a str, &'a [u8])>) -> String {
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

/// The deadline a capture gets, or `None` to keep it until it ages out of
/// the history. A password-manager copy is never kept permanently, whatever
/// the policy says: it is skipped instead.
fn deadline_for(
    disposition: Disposition,
    password_marked: bool,
    copied_at: i64,
    timeout: i64,
) -> Result<Option<i64>, Skipped> {
    match disposition {
        Disposition::Skip => Err(Skipped::Excluded),
        Disposition::Temporary => Ok(Some(copied_at.saturating_add(timeout))),
        Disposition::Normal if password_marked => Err(Skipped::Failed(
            "password-manager copies are never recorded permanently".into(),
        )),
        Disposition::Normal => Ok(None),
    }
}

fn kind_of(mimes: &[&str]) -> ItemKind {
    let known = |name: &str| mimes.iter().any(|mime| recognized_mime(mime) == Some(name));
    if mimes.iter().any(|mime| mime.starts_with("image/")) {
        ItemKind::Image
    } else if known("text/uri-list") {
        ItemKind::Files
    } else if known("text/plain") {
        ItemKind::Text
    } else if known("text/html") {
        ItemKind::Html
    } else {
        ItemKind::Other
    }
}

/// The name of a text type Raffi knows, whatever case and parameters it came
/// with: a browser offers `text/html;charset=utf-8` for an HTML entry.
pub(crate) fn recognized_mime(mime: &str) -> Option<&'static str> {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    ["text/plain", "text/html", "text/uri-list"]
        .into_iter()
        .find(|known| base.eq_ignore_ascii_case(known))
}

/// The name a format is stored and looked up under: a recognized text type
/// loses its parameters and case, anything else is stored as it came.
pub(crate) fn canonical_mime(mime: &str) -> String {
    let mime = mime.trim();
    match recognized_mime(mime) {
        Some(known) => known.to_string(),
        None => mime.to_string(),
    }
}

/// Whether `mime` is a name a selection can offer: `type/subtype` of RFC 6838
/// token characters, optional parameters after a `;`, no control character
/// and at most [`MAX_MIME_BYTES`] bytes. Wayland turns the name into a C
/// string, so a NUL would abort the process that offers it.
pub(crate) fn validate_mime(mime: &str) -> Result<(), String> {
    let mime = mime.trim();
    if mime.is_empty() {
        return Err("a MIME type cannot be empty".to_string());
    }
    if mime.chars().any(char::is_control) {
        return Err("a MIME type cannot hold a control character".to_string());
    }
    if mime.len() > MAX_MIME_BYTES {
        return Err(format!(
            "a MIME type cannot be longer than {MAX_MIME_BYTES} bytes"
        ));
    }
    let (base, parameters) = match mime.split_once(';') {
        Some((base, parameters)) => (base.trim(), Some(parameters)),
        None => (mime, None),
    };
    let token = |value: &str| !value.is_empty() && value.chars().all(is_token_char);
    let Some((kind, subtype)) = base.split_once('/') else {
        return Err("a MIME type needs a `type/subtype` shape".to_string());
    };
    if !token(kind) || !token(subtype) {
        return Err("the type and subtype of a MIME type are letters, digits and the \
                    symbols `!#$%&'*+-.^_`|~`"
            .to_string());
    }
    if let Some(parameters) = parameters {
        if parameters.trim().is_empty() {
            return Err("the parameters of a MIME type cannot be empty".to_string());
        }
    }
    Ok(())
}

/// Characters an RFC 7230 token is made of, which MIME type and subtype names
/// are a subset of.
fn is_token_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(
            character,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '.'
                | '^'
                | '_'
                | '`'
                | '|'
                | '~'
        )
}

pub(crate) fn kind_name(kind: ItemKind) -> &'static str {
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
    super::clipboard_policy::matches_any(text, patterns)
}

/// Searchable text: plain text, else the file list, else the HTML with its
/// tags removed. A recognized type counts whatever parameters it came with.
pub(crate) fn text_of(formats: &[(String, Vec<u8>)]) -> String {
    let format = |name: &str| {
        formats
            .iter()
            .find(|(mime, data)| !data.is_empty() && recognized_mime(mime) == Some(name))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("raffi-clipstore-{name}-{unique}"))
    }

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = temp_dir(name);
        let store = Store::open(&dir.join("clipboard.db")).unwrap();
        (store, dir)
    }

    /// Clock injected into the operations that care about time; entries
    /// recorded with `text` never expire.
    const NOW: i64 = 10_000;

    const LIMITS: Limits = Limits {
        max_items: 100,
        max_size: 1 << 30,
        max_image_size: 1 << 20,
    };

    /// A policy recording everything as temporary for `timeout` seconds.
    fn temporary_policy(timeout: i64) -> Policy {
        Policy {
            temporary: vec![regex::Regex::new("").unwrap()],
            temporary_timeout: timeout,
            ..Policy::default()
        }
    }

    fn deadline(store: &Store, hash: &str) -> Option<i64> {
        store
            .conn
            .query_row(
                "SELECT expires_at FROM items WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// (entries, stored formats) in the database, expired ones included.
    fn rows(store: &Store) -> (i64, i64) {
        let count = |table: &str| -> i64 {
            store
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        (count("items"), count("formats"))
    }

    fn text(value: &str, at: i64) -> Capture {
        Capture {
            formats: vec![("text/plain".into(), value.as_bytes().to_vec())],
            copied_at: at,
            source: Some("kitty".into()),
            password_marked: false,
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
        let hash = store
            .insert(text("selected entry", 1), &LIMITS, NOW)
            .unwrap();
        let path = dir.join("clipboard.db");
        let selector = std::thread::spawn(move || {
            let other = Store::open(&path).unwrap();
            for _ in 0..200 {
                other.bump(&hash).unwrap();
            }
        });
        for index in 0..200 {
            store
                .insert(text(&format!("copy number {index}"), 2), &LIMITS, NOW)
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
        let first = store
            .insert(text("first entry", 100), &LIMITS, NOW)
            .unwrap();
        store
            .insert(text("second entry", 200), &LIMITS, NOW)
            .unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[], NOW).unwrap()),
            ["second entry", "first entry"]
        );

        let again = store
            .insert(text("first entry", 300), &LIMITS, NOW)
            .unwrap();
        assert_eq!(again, first);
        let page = store.list("", "", 0, &[], NOW).unwrap();
        assert_eq!(titles(&page), ["first entry", "second entry"]);
        assert_eq!(page.total, 2);
        assert_eq!(page.head, first);
        assert_eq!(page.items[0].copied, Some(300));
        assert_eq!(page.items[0].source.as_deref(), Some("kitty"));

        store.bump(&page.items[1].id).unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[], NOW).unwrap()),
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
            store
                .insert(text(value, index as i64), &limits, NOW)
                .unwrap();
        }
        // Three fit by count, but only two fit in 25 bytes.
        assert_eq!(
            titles(&store.list("", "", 0, &[], NOW).unwrap()),
            ["dddddddddd", "cccccccccc"]
        );

        let tiny = Limits {
            max_size: 1,
            ..limits
        };
        store.insert(text("eeeeeeeeee", 9), &tiny, NOW).unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[], NOW).unwrap()),
            ["eeeeeeeeee"]
        );
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
            password_marked: false,
        };
        assert!(matches!(
            store.insert(capture, &limits, NOW),
            Err(Skipped::TooLarge(_))
        ));
        let empty = Capture {
            formats: vec![("text/plain".into(), Vec::new())],
            copied_at: 1,
            source: None,
            password_marked: false,
        };
        assert_eq!(store.insert(empty, &limits, NOW), Err(Skipped::Empty));
        assert_eq!(store.list("", "", 0, &[], NOW).unwrap().total, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_search_folds_unicode_case_and_ignores_surrounding_space() {
        let (mut store, dir) = temp_store("search-unicode");
        for (index, value) in ["  Été à PARIS\n", "visite de paris en été", "Straße"]
            .into_iter()
            .enumerate()
        {
            store
                .insert(text(value, index as i64), &LIMITS, NOW)
                .unwrap();
        }
        let page = store.list("éTÉ À paris", "", 0, &[], NOW).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].tier, 0);
        let page = store.list("PARIS été", "", 0, &[], NOW).unwrap();
        let tiers: Vec<u8> = page.items.iter().map(|item| item.tier).collect();
        assert_eq!(tiers, [2, 2]);
        assert_eq!(
            titles(&store.list("STRAßE", "", 0, &[], NOW).unwrap()),
            ["Straße"]
        );
        assert!(store
            .list("nowhere", "", 0, &[], NOW)
            .unwrap()
            .items
            .is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_replaced_listing_stops() {
        let (mut store, dir) = temp_store("search-cancel");
        store
            .conn
            .execute_batch(
                "INSERT INTO items (hash, seq, kind, formats, text, text_len, trim_len, is_url,
                     copied_at, size, search)
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
                 SELECT 'h' || i, i, 'text', 'text/plain', 'entry ' || i, 10, 10, 0, 1, 10,
                     'entry ' || i
                 FROM n;",
            )
            .unwrap();
        let replaced: Cancel = std::sync::Arc::new(|| true);
        assert!(store
            .list_until("entry", "", 0, &[], NOW, Some(replaced))
            .is_err());

        // Replaced while SQLite is scanning: the progress handler stops it.
        let checks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = checks.clone();
        let mid_scan: Cancel = std::sync::Arc::new(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 2
        });
        let error = store
            .list_until("entry 1", "", 0, &[], NOW, Some(mid_scan))
            .unwrap_err();
        assert!(error.contains("interrupt"), "{error}");
        assert!(checks.load(std::sync::atomic::Ordering::Relaxed) >= 3);

        // The connection lists and records normally afterwards.
        let current: Cancel = std::sync::Arc::new(|| false);
        let page = store
            .list_until("entry 19999", "", 0, &[], NOW, Some(current))
            .unwrap();
        assert_eq!(titles(&page), ["entry 19999"]);
        store.insert(text("after", 2), &LIMITS, NOW).unwrap();
        assert_eq!(
            titles(&store.list("after", "", 0, &[], NOW).unwrap()),
            ["after"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_concurrent_opens_upgrade_an_old_database_once() {
        let dir = temp_dir("upgrade-race");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clipboard.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE items (
                 id INTEGER PRIMARY KEY, hash TEXT NOT NULL UNIQUE, seq INTEGER NOT NULL,
                 kind TEXT NOT NULL, formats TEXT NOT NULL, text TEXT NOT NULL,
                 text_len INTEGER NOT NULL, trim_len INTEGER NOT NULL, is_url INTEGER NOT NULL,
                 copied_at INTEGER NOT NULL, source TEXT, size INTEGER NOT NULL,
                 expires_at INTEGER
             );
             CREATE TABLE formats (
                 item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
                 mime TEXT NOT NULL, data BLOB NOT NULL, PRIMARY KEY (item_id, mime)
             );
             PRAGMA user_version = 2;
             INSERT INTO items (hash, seq, kind, formats, text, text_len, trim_len, is_url,
                     copied_at, source, size)
             VALUES ('old', 1, 'text', 'text/plain', 'Kept Entry', 10, 10, 0, 900, NULL, 10);",
        )
        .unwrap();
        drop(conn);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
        let openers: Vec<_> = (0..6)
            .map(|_| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&path).map(|_| ())
                })
            })
            .collect();
        for opener in openers {
            opener.join().unwrap().unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            titles(&store.list("kept", "", 0, &[], NOW).unwrap()),
            ["Kept Entry"]
        );
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
            store
                .insert(text(value, index as i64), &LIMITS, NOW)
                .unwrap();
        }
        let image = Capture {
            formats: vec![
                ("image/png".into(), png(3, 2)),
                ("text/html".into(), b"<img src=\"x.png\">".to_vec()),
            ],
            copied_at: 10,
            source: Some("firefox".into()),
            password_marked: false,
        };
        store.insert(image, &LIMITS, NOW).unwrap();

        let page = store.list("Hello  World", "", 0, &[], NOW).unwrap();
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

        let images = store.list("", "image", 0, &[], NOW).unwrap();
        assert_eq!(images.items.len(), 1);
        assert_eq!(images.items[0].kind, ItemKind::Image);
        assert_eq!(images.items[0].title(), "Image (PNG)");

        let urls = store.list("", "url", 0, &[], NOW).unwrap();
        assert_eq!(titles(&urls), ["https://example.com/page"]);

        let all = store.list("", "", 3, &[], NOW).unwrap();
        assert!(!titles(&all).contains(&"ok".to_string()));
        assert_eq!(all.items.len(), 6);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_password_marked_capture_is_temporary_without_matching_a_pattern() {
        let (mut store, dir) = temp_store("password-marked");
        let policy = Policy {
            temporary_password_manager: true,
            ..Policy::default()
        };
        let capture = Capture {
            password_marked: true,
            ..text("hunter2", 1_000)
        };
        let hash = store
            .insert_filtered(capture, &LIMITS, &policy, 1_000)
            .unwrap();
        // The default timeout applies, with no temporary pattern involved.
        assert_eq!(deadline(&store, &hash), Some(1_060));
        // Selecting it from the browser does not extend the deadline.
        store.bump(&hash).unwrap();
        assert_eq!(deadline(&store, &hash), Some(1_060));
        assert_eq!(sweep(&mut store, 1_060).unwrap(), 1);
        assert_eq!(rows(&store), (0, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_password_marked_capture_keeps_the_marker_without_counting_as_content() {
        let (mut store, dir) = temp_store("password-marker");
        let policy = Policy {
            temporary_password_manager: true,
            ..Policy::default()
        };

        // The source offered the marker with its usual payload.
        let hash = store
            .insert_filtered(
                Capture {
                    formats: vec![
                        ("text/plain".into(), b"hunter2".to_vec()),
                        (PASSWORD_HINT.into(), b"secret".to_vec()),
                    ],
                    password_marked: true,
                    ..text("hunter2", 1_000)
                },
                &LIMITS,
                &policy,
                1_000,
            )
            .unwrap();
        // Served again with the hint, so paste targets still treat it as a
        // secret, and the marker payload is the source's own.
        assert_eq!(
            store.formats(&hash, 1_000).unwrap(),
            [
                ("text/plain".to_string(), b"hunter2".to_vec()),
                (PASSWORD_HINT.to_string(), b"secret".to_vec()),
            ]
        );
        // ... but it is not text, a kind or a listed format.
        let item = &store.list("", "", 0, &[], 1_000).unwrap().items[0];
        assert_eq!(item.kind, ItemKind::Text);
        assert_eq!(item.formats, ["text/plain"]);
        assert_eq!(item.title(), "hunter2");
        let preview = store.preview(&hash, 1_000).unwrap();
        assert_eq!(preview.formats, [("text/plain".to_string(), 7)]);
        assert_eq!(preview.text, "hunter2");

        // A source that offers the marker with no data still leaves one that
        // survives the empty-format filter.
        let empty_marker = store
            .insert_filtered(
                Capture {
                    formats: vec![
                        ("text/plain".into(), b"hunter3".to_vec()),
                        (PASSWORD_HINT.into(), Vec::new()),
                    ],
                    password_marked: true,
                    ..text("hunter3", 1_010)
                },
                &LIMITS,
                &policy,
                1_010,
            )
            .unwrap();
        let formats = store.formats(&empty_marker, 1_010).unwrap();
        assert_eq!(formats[1].0, PASSWORD_HINT);
        assert!(!formats[1].1.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_the_latest_copy_decides_the_deadline_and_the_marker() {
        let (mut store, dir) = temp_store("password-duplicate");
        let policy = Policy {
            temporary_password_manager: true,
            ..Policy::default()
        };
        let hash = store.insert(text("hunter2", 1_000), &LIMITS, NOW).unwrap();
        assert_eq!(deadline(&store, &hash), None);

        // The same text copied again as a password-manager secret: one entry,
        // now temporary and served with the hint.
        let again = store
            .insert_filtered(
                Capture {
                    password_marked: true,
                    ..text("hunter2", 1_050)
                },
                &LIMITS,
                &policy,
                1_050,
            )
            .unwrap();
        assert_eq!(again, hash);
        assert_eq!(deadline(&store, &hash), Some(1_110));
        assert!(store
            .formats(&hash, 1_050)
            .unwrap()
            .iter()
            .any(|(mime, data)| mime == PASSWORD_HINT && !data.is_empty()));
        assert_eq!(store.list("", "", 0, &[], 1_050).unwrap().items.len(), 1);

        // Copied once more without the marker: the entry is permanent and the
        // hint is gone.
        store
            .insert_filtered(text("hunter2", 1_100), &LIMITS, &policy, 1_100)
            .unwrap();
        assert_eq!(deadline(&store, &hash), None);
        assert_eq!(
            store.formats(&hash, 1_100).unwrap(),
            [("text/plain".to_string(), b"hunter2".to_vec())]
        );
        assert_eq!(sweep(&mut store, i64::MAX).unwrap(), 0);
        assert_eq!(store.list("", "", 0, &[], i64::MAX).unwrap().items.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_password_marked_capture_is_never_kept_permanently() {
        // The policy always makes a marked capture temporary; this guard is
        // what keeps a misconfigured one out of the history for good.
        assert_eq!(
            deadline_for(Disposition::Normal, true, 1_000, 60),
            Err(Skipped::Failed(
                "password-manager copies are never recorded permanently".into()
            ))
        );
        assert_eq!(
            deadline_for(Disposition::Normal, false, 1_000, 60),
            Ok(None)
        );
        assert_eq!(
            deadline_for(Disposition::Temporary, true, 1_000, 60),
            Ok(Some(1_060))
        );
        assert_eq!(
            deadline_for(Disposition::Skip, true, 1_000, 60),
            Err(Skipped::Excluded)
        );
    }

    #[test]
    fn test_exclusions_win_over_the_password_manager_opt_in() {
        let (mut store, dir) = temp_store("password-excluded");
        let policy = Policy {
            exclude: vec![regex::Regex::new("(?i)^hunter").unwrap()],
            temporary_password_manager: true,
            ..Policy::default()
        };
        let capture = Capture {
            password_marked: true,
            ..text("hunter2", 1_000)
        };
        assert_eq!(
            store.insert_filtered(capture, &LIMITS, &policy, 1_000),
            Err(Skipped::Excluded)
        );
        assert_eq!(rows(&store), (0, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_insert_filtered_checks_text_after_normalization() {
        let (mut store, dir) = temp_store("normalized-exclude");
        let policies = [
            Policy {
                exclude: vec![regex::Regex::new("(?i)password").unwrap()],
                ..Policy::default()
            },
            Policy {
                temporary: vec![regex::Regex::new("(?i)password").unwrap()],
                temporary_password_manager: true,
                ..Policy::default()
            },
            Policy {
                temporary_password_manager: true,
                ..Policy::default()
            },
        ];
        let capture = Capture {
            formats: vec![
                ("text/plain".into(), Vec::new()),
                ("text/html".into(), b"<p>Password = x</p>".to_vec()),
            ],
            copied_at: NOW - 10,
            source: None,
            password_marked: true,
        };

        assert_eq!(
            store.insert_filtered(capture.clone(), &LIMITS, &policies[0], NOW),
            Err(Skipped::Excluded)
        );
        assert!(store.list("", "", 0, &[], NOW).unwrap().items.is_empty());
        // A temporary entry is still recorded, and so is a password-marked
        // one once the password manager opt-in is set.
        assert!(store
            .insert_filtered(capture.clone(), &LIMITS, &policies[1], NOW)
            .is_ok());
        assert!(store
            .insert_filtered(capture, &LIMITS, &policies[2], NOW)
            .is_ok());
        assert_eq!(store.list("", "", 0, &[], NOW).unwrap().items.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_temporary_entry_expires_at_its_deadline() {
        let (mut store, dir) = temp_store("expiry-boundary");
        let policy = temporary_policy(60);
        let hash = store
            .insert_filtered(text("code: 1234", 1_000), &LIMITS, &policy, 1_000)
            .unwrap();
        assert_eq!(deadline(&store, &hash), Some(1_060));

        // A second before the deadline it is still listed, previewable and
        // selectable, and the listing carries the deadline.
        let listed = store.list("", "", 0, &[], 1_059).unwrap();
        assert_eq!(titles(&listed), ["code: 1234"]);
        assert_eq!(listed.items[0].expires_at, Some(1_060));
        let permanent = store
            .insert_filtered(text("plain", 1_000), &LIMITS, &Policy::default(), 1_000)
            .unwrap();
        let listed = store.list("", "", 0, &[], 1_059).unwrap();
        let stays = listed
            .items
            .iter()
            .find(|item| item.id == permanent)
            .expect("the entry without a deadline");
        assert_eq!(stays.expires_at, None);
        store.delete(&permanent).unwrap();
        assert!(store.preview(&hash, 1_059).is_ok());
        assert!(store.formats(&hash, 1_059).is_ok());
        assert_eq!(sweep(&mut store, 1_059).unwrap(), 0);

        // At the deadline it is gone, before any cleanup has run.
        let page = store.list("", "", 0, &[], 1_060).unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, 0);
        let gone = "item is no longer in the clipboard history";
        assert_eq!(store.preview(&hash, 1_060).unwrap_err(), gone);
        assert_eq!(store.formats(&hash, 1_060).unwrap_err(), gone);

        assert_eq!(rows(&store), (1, 1));
        assert_eq!(sweep(&mut store, 1_060).unwrap(), 1);
        assert_eq!(rows(&store), (0, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_capture_that_already_expired_is_not_recorded() {
        let (mut store, dir) = temp_store("expired-capture");
        let policy = temporary_policy(60);
        assert_eq!(
            store.insert_filtered(text("code: 1234", 1_000), &LIMITS, &policy, 1_060),
            Err(Skipped::Expired)
        );
        assert_eq!(
            store.insert_filtered(text("code: 1234", 1_000), &LIMITS, &policy, 1_061),
            Err(Skipped::Expired)
        );
        assert_eq!(rows(&store), (0, 0));
        // A second earlier it still has a lifetime left.
        assert!(store
            .insert_filtered(text("code: 1234", 1_000), &LIMITS, &policy, 1_059)
            .is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_duplicate_copies_reset_the_deadline() {
        let (mut store, dir) = temp_store("expiry-duplicate");
        let policy = temporary_policy(60);
        let hash = store
            .insert_filtered(text("code: 1234", 1_000), &LIMITS, &policy, 1_000)
            .unwrap();
        assert_eq!(deadline(&store, &hash), Some(1_060));

        // Copied again as temporary: a deadline counted from the new copy.
        let again = store
            .insert_filtered(text("code: 1234", 1_050), &LIMITS, &policy, 1_050)
            .unwrap();
        assert_eq!(again, hash);
        assert_eq!(deadline(&store, &hash), Some(1_110));
        assert_eq!(store.list("", "", 0, &[], 1_100).unwrap().items.len(), 1);

        // Copied again as a normal entry: no deadline any more.
        store
            .insert_filtered(
                text("code: 1234", 1_110),
                &LIMITS,
                &Policy::default(),
                1_110,
            )
            .unwrap();
        assert_eq!(deadline(&store, &hash), None);
        assert_eq!(sweep(&mut store, i64::MAX).unwrap(), 0);
        assert_eq!(store.list("", "", 0, &[], i64::MAX).unwrap().items.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_selecting_an_entry_does_not_extend_its_deadline() {
        let (mut store, dir) = temp_store("expiry-bump");
        let policy = temporary_policy(60);
        let hash = store
            .insert_filtered(text("code: 1234", 1_000), &LIMITS, &policy, 1_000)
            .unwrap();
        store.bump(&hash).unwrap();
        assert_eq!(deadline(&store, &hash), Some(1_060));
        assert_eq!(store.list("", "", 0, &[], 1_059).unwrap().items.len(), 1);
        assert_eq!(sweep(&mut store, 1_060).unwrap(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_new_configuration_leaves_existing_deadlines_alone() {
        let (mut store, dir) = temp_store("expiry-config");
        let hash = store
            .insert_filtered(
                text("code: 1234", 1_000),
                &LIMITS,
                &temporary_policy(60),
                1_000,
            )
            .unwrap();
        // A longer timeout only applies to what is recorded after it.
        store
            .insert_filtered(
                text("code: 5678", 1_000),
                &LIMITS,
                &temporary_policy(600),
                1_000,
            )
            .unwrap();
        assert_eq!(deadline(&store, &hash), Some(1_060));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cleanup_runs_without_new_captures() {
        let (mut store, dir) = temp_store("expiry-sweep");
        let policy = temporary_policy(30);
        store
            .insert_filtered(text("code: 1", 1_000), &LIMITS, &policy, 1_000)
            .unwrap();
        store
            .insert_filtered(text("code: 2", 1_010), &LIMITS, &policy, 1_010)
            .unwrap();
        store
            .insert(text("kept entry", 1_000), &LIMITS, 1_000)
            .unwrap();

        assert_eq!(sweep(&mut store, 1_029).unwrap(), 0);
        // The sweeper keeps going between copies: only the due entry goes.
        assert_eq!(sweep(&mut store, 1_030).unwrap(), 1);
        assert_eq!(sweep(&mut store, 1_030).unwrap(), 0);
        assert_eq!(
            titles(&store.list("", "", 0, &[], 1_030).unwrap()),
            ["kept entry", "code: 2"]
        );
        assert_eq!(sweep(&mut store, 1_040).unwrap(), 1);
        assert_eq!(
            titles(&store.list("", "", 0, &[], 1_040).unwrap()),
            ["kept entry"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_expiring_the_newest_entry_takes_every_format() {
        let (mut store, dir) = temp_store("expiry-formats");
        let policy = temporary_policy(60);
        let older = store
            .insert(text("older entry", 1_000), &LIMITS, 1_000)
            .unwrap();
        let newest = store
            .insert_filtered(
                Capture {
                    formats: vec![
                        ("text/html".into(), b"<p>code 4242</p>".to_vec()),
                        ("text/plain".into(), b"code 4242".to_vec()),
                    ],
                    copied_at: 1_000,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                &policy,
                1_000,
            )
            .unwrap();
        assert_eq!(rows(&store), (2, 3));

        assert_eq!(sweep(&mut store, 1_060).unwrap(), 1);
        assert_eq!(rows(&store), (1, 1));
        assert!(store.formats(&newest, 1_060).is_err());
        assert!(store.formats(&older, 1_060).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_pruning_ignores_expired_entries() {
        let (mut store, dir) = temp_store("expiry-prune");
        let limits = Limits {
            max_items: 2,
            ..LIMITS
        };
        store
            .insert(text("older entry", 1_000), &limits, 1_000)
            .unwrap();
        store
            .insert_filtered(
                text("code: 1234", 1_000),
                &limits,
                &temporary_policy(60),
                1_000,
            )
            .unwrap();

        // At 1_070 the temporary entry is expired: it neither counts for the
        // history size nor takes the place of the newest live entry.
        store
            .insert(text("newer entry", 1_070), &limits, 1_070)
            .unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[], 1_070).unwrap()),
            ["newer entry", "older entry"]
        );
        assert_eq!(rows(&store), (3, 3));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_version_1_database_is_upgraded_without_losing_history() {
        let dir = temp_dir("upgrade");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clipboard.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE items (
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
             CREATE INDEX items_seq ON items (seq);
             CREATE INDEX items_kind ON items (kind, seq);
             CREATE TABLE formats (
                 item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
                 mime TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (item_id, mime)
             );
             PRAGMA user_version = 1;
             INSERT INTO items (hash, seq, kind, formats, text, text_len, trim_len, is_url,
                     copied_at, source, size)
             VALUES ('old', 1, 'text', 'text/plain', 'kept entry', 10, 10, 0, 900, 'kitty', 10);
             INSERT INTO formats (item_id, mime, data) VALUES (1, 'text/plain', 'kept entry');",
        )
        .unwrap();
        drop(conn);

        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            titles(&store.list("", "", 0, &[], NOW).unwrap()),
            ["kept entry"]
        );
        // The search column is filled in for the existing rows.
        assert_eq!(
            titles(&store.list("KEPT", "", 0, &[], NOW).unwrap()),
            ["kept entry"]
        );
        assert_eq!(deadline(&store, "old"), None);
        assert_eq!(sweep(&mut store, i64::MAX).unwrap(), 0);
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(store);

        // Opening it again changes nothing.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list("", "", 0, &[], NOW).unwrap().items.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_removes_every_entry_and_scrubs_the_log() {
        let (mut store, dir) = temp_store("clear");
        let secret = "zq-cleared-secret-1234";
        store.insert(text(secret, 1), &LIMITS, NOW).unwrap();
        store
            .insert(text("keep me for now", 2), &LIMITS, NOW)
            .unwrap();
        store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png(3, 2))],
                    copied_at: 3,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        store
            .insert_filtered(text("code: 9999", NOW), &LIMITS, &temporary_policy(60), NOW)
            .unwrap();
        assert_eq!(rows(&store), (4, 4));

        // A reader still scanning keeps the log from being emptied.
        let reader = Connection::open(dir.join("clipboard.db")).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM items;")
            .unwrap();
        let (removed, emptied) = store.clear("1").unwrap();
        assert_eq!(removed, 4);
        assert!(!emptied, "a concurrent reader keeps the log");
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        // The retry the CLI makes empties it, with this connection still open.
        assert!(store.truncate_wal());
        assert_eq!(rows(&store), (0, 0));
        for name in ["clipboard.db", "clipboard.db-wal"] {
            let bytes = fs::read(dir.join(name)).unwrap_or_default();
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "{secret} is still in {name}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_can_keep_one_kind() {
        let (mut store, dir) = temp_store("clear-kind");
        store.insert(text("an entry", 1), &LIMITS, NOW).unwrap();
        store.insert(text("short", 2), &LIMITS, NOW).unwrap();
        store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png(3, 2))],
                    copied_at: 3,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let (removed, _) = store.clear(kind_predicate("image").unwrap()).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            titles(&store.list("", "", 0, &[], NOW).unwrap()),
            ["short", "an entry"]
        );
        // Only the image went, formats and all.
        assert_eq!(rows(&store), (2, 2));

        // Exclusions are not eligibility here: clear takes hidden entries too.
        let exclude = [regex::Regex::new("^an").unwrap()];
        assert_eq!(
            titles(&store.list("", "", 0, &exclude, NOW).unwrap()),
            ["short"]
        );
        assert_eq!(store.count("1", NOW).unwrap(), (2, 2));
        assert_eq!(store.clear("1").unwrap().0, 2);
        assert_eq!(rows(&store), (0, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_kind_predicates_are_explicit() {
        assert_eq!(kind_predicate("text"), Some("kind = 'text'"));
        assert_eq!(kind_predicate("html"), Some("kind = 'html'"));
        assert_eq!(kind_predicate("files"), Some("kind = 'files'"));
        assert_eq!(kind_predicate("other"), Some("kind = 'other'"));
        assert_eq!(kind_predicate("image"), Some("kind = 'image'"));
        assert_eq!(
            kind_predicate("url"),
            Some("kind IN ('text', 'html') AND is_url = 1")
        );
        assert_eq!(kind_predicate("images"), None);
        assert_eq!(kind_predicate(""), None);
        assert_eq!(
            DocumentedKinds::ALL,
            ["text", "html", "image", "files", "url", "other"],
            "the CLI accepts exactly these"
        );
    }

    /// The `-k` values, so a new one has to be added here too.
    struct DocumentedKinds;

    impl DocumentedKinds {
        const ALL: [&'static str; 6] = ["text", "html", "image", "files", "url", "other"];
    }

    #[test]
    fn test_counts_report_the_live_entries() {
        let (mut store, dir) = temp_store("counts");
        store.insert(text("kept entry", 1), &LIMITS, NOW).unwrap();
        store
            .insert_filtered(text("code: 1234", NOW), &LIMITS, &temporary_policy(60), NOW)
            .unwrap();
        let counts = store.counts(NOW).unwrap();
        assert_eq!((counts.total, counts.alive, counts.temporary), (2, 2, 1));
        assert_eq!(counts.bytes, 10 + 10);
        // The temporary entry is gone from the history, the row is not.
        let counts = store.counts(NOW + 60).unwrap();
        assert_eq!((counts.total, counts.alive, counts.temporary), (2, 1, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_opening_an_absent_history_creates_nothing() {
        // `default_path` refuses in tests, so this covers the branch the CLI
        // takes on an installation without a database.
        assert!(!exists());
        assert!(Store::open_existing().is_err());
        assert_eq!(disk_usage(), 0);

        let dir = temp_dir("opening");
        let path = dir.join("clipboard.db");
        let mut store = Store::open(&path).unwrap();
        store.insert(text("an entry", 1), &LIMITS, NOW).unwrap();
        drop(store);
        // Opening an existing file works; a missing one is not created here.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.count("1", NOW).unwrap(), (1, 1));
        drop(store);
        let opened = Store::open_existing_at(&path).unwrap().unwrap();
        assert_eq!(opened.count("1", NOW).unwrap(), (1, 1));
        drop(opened);
        let absent = dir.join("absent.db");
        assert!(!absent.exists());
        assert!(Store::open_existing_at(&absent).unwrap().is_none());
        assert!(!absent.exists(), "the check must not create the file");
        let orphan = dir.join("missing").join("clipboard.db");
        assert!(Store::open_existing_at(&orphan).unwrap().is_none());
        assert!(
            !dir.join("missing").exists(),
            "the check must not create the directory"
        );
        // Even when the file vanished after the check, the open cannot put
        // an empty database back in its place.
        assert!(Store::open_read_write(&absent).is_err());
        assert!(!absent.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_expired_and_deleted_content_is_scrubbed_from_disk() {
        let (mut store, dir) = temp_store("scrub");
        store.conn.busy_timeout(Duration::from_millis(50)).unwrap();
        let secret = "zq-expiring-secret-4711";
        store
            .insert_filtered(text(secret, 1_000), &LIMITS, &temporary_policy(60), 1_000)
            .unwrap();
        let deleted = "zq-deleted-entry-0815";
        let hash = store.insert(text(deleted, 1_001), &LIMITS, 1_001).unwrap();
        assert_eq!(store.expire(1_059).unwrap(), 0, "nothing is due yet");

        // A listing still reading keeps the log from being emptied.
        let reader = Connection::open(dir.join("clipboard.db")).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM items;")
            .unwrap();
        assert_eq!(store.expire(1_060).unwrap(), 1);
        store.delete(&hash).unwrap();
        reader.execute_batch("COMMIT").unwrap();

        // The sweeper empties it on its next tick, with both connections
        // still open so closing them cannot be what cleans up.
        assert_eq!(sweep(&mut store, 1_061).unwrap(), 0);
        for name in ["clipboard.db", "clipboard.db-wal"] {
            let bytes = fs::read(dir.join(name)).unwrap_or_default();
            for gone in [secret, deleted] {
                assert!(
                    !bytes
                        .windows(gone.len())
                        .any(|window| window == gone.as_bytes()),
                    "{gone} is still in {name}"
                );
            }
        }
        drop(reader);
        drop(store);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_the_deadline_survives_a_restart() {
        let (mut store, dir) = temp_store("expiry-restart");
        let hash = store
            .insert_filtered(
                text("code: 1234", 1_000),
                &LIMITS,
                &temporary_policy(60),
                1_000,
            )
            .unwrap();
        drop(store);

        let mut store = Store::open(&dir.join("clipboard.db")).unwrap();
        assert_eq!(deadline(&store, &hash), Some(1_060));
        assert_eq!(store.list("", "", 0, &[], 1_059).unwrap().items.len(), 1);
        assert_eq!(sweep(&mut store, 1_060).unwrap(), 1);
        assert_eq!(rows(&store), (0, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_list_hides_entries_matching_exclusions() {
        let (mut store, dir) = temp_store("exclude");
        store
            .insert(text("secret: hunter2", 1), &LIMITS, NOW)
            .unwrap();
        store
            .insert(text("line one\nPASSWORD=abc", 2), &LIMITS, NOW)
            .unwrap();
        store
            .insert(
                Capture {
                    formats: vec![("text/html".into(), b"<p>Password = x</p>".to_vec())],
                    copied_at: 3,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let long = format!("{}needle", "a".repeat(EXCERPT_CHARS + 50));
        store.insert(text(&long, 4), &LIMITS, NOW).unwrap();
        store
            .insert(text("keep this entry", 5), &LIMITS, NOW)
            .unwrap();

        let exclude = [
            regex::Regex::new("^secret:").unwrap(),
            regex::Regex::new("(?i)password\\s*=").unwrap(),
            regex::Regex::new("needle").unwrap(),
        ];
        let page = store.list("", "", 0, &exclude, NOW).unwrap();
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
            .insert(text("line one\nline two", 1), &LIMITS, NOW)
            .unwrap();
        let preview = store.preview(&text_id, NOW).unwrap();
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
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let preview = store.preview(&image_id, NOW).unwrap();
        assert_eq!(preview.kind, ItemKind::Image);
        let image = preview.image.unwrap().unwrap();
        assert_eq!(
            (image.mime.as_str(), image.width, image.height),
            ("image/png", 5, 3)
        );
        assert_eq!(
            store.formats(&image_id, NOW).unwrap(),
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
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let preview = store.preview(&files_id, NOW).unwrap();
        assert_eq!(preview.kind, ItemKind::Files);
        assert_eq!(preview.files[0].name, "/definitely/missing file");
        assert_eq!(preview.files[0].missing, Some(true));

        store.delete(&text_id).unwrap();
        assert!(store.preview(&text_id, NOW).is_err());
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
