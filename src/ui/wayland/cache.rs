//! Private snapshot caches and the optional background server.
//!
//! Snapshots are versioned JSON files under `$XDG_CACHE_HOME/raffi`, written
//! atomically with private permissions. The server speaks newline-delimited
//! JSON over a Unix socket; when it is absent the UI reads and writes the same
//! files directly.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{AddonsConfig, ParsedConfig, ScriptFilterConfig};

use super::clipboard_store::MAX_MIME_BYTES;
use super::script_filters::{self, PreviewRequest};
use super::types::{ScriptFilterResult, ScriptFilterView};
use super::warm_schedule::{advertised_queries, Policy, Run, Warmer, MAX_WARM_VIEWS};

const SNAPSHOT_VERSION: u32 = 1;
/// Snapshots older than this are discarded instead of shown while refreshing.
pub(super) const MAX_STALE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LIST_KEEP: usize = 200;
const PREVIEW_KEEP: usize = 200;
const SERVER_CLIENT_TIMEOUT: Duration = Duration::from_millis(100);
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// Waited refreshes run Slack and script filters, which can be slow.
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const CLIPBOARD_NOT_RUNNING: &str =
    "Raffi clipboard server is not running: start `raffi --server`";
const WARM_TICK: Duration = Duration::from_secs(30);
/// Longest a script filter run for the server may take. Shorter than
/// `WAIT_TIMEOUT`, so a client waiting on F5 hears about it first.
const SERVER_RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Protocol revision reported by `Status`; servers from before it existed
/// report 0 and ignore `wait`. Revision 2 adds `clipboard_select`, revision
/// 3 the `clipboard` status flag, revision 4 `clipboard_set`, and revision 5
/// its `expires_in`. Revision 6 preserves the cache on `cache.seconds: 0`.
pub(crate) const PROTOCOL: u32 = 6;
const CLIPBOARD_SET_PROTOCOL: u32 = 5;
/// Upper bound on the formats one `clipboard set` request may carry.
const MAX_SET_FORMATS: usize = 32;
/// Largest single format a request may carry, whatever the configured image
/// limit: the server has to bound the line it reads before decoding it.
const MAX_SET_FORMAT_BYTES: u64 = 64 * 1024 * 1024;
/// Longest request line the server reads: one format at
/// [`MAX_SET_FORMAT_BYTES`], base64-encoded, with the MIME names and the JSON
/// framing of [`MAX_SET_FORMATS`] formats around it.
const MAX_REQUEST_LINE: usize =
    base64_len(MAX_SET_FORMAT_BYTES as usize) + MAX_SET_FORMATS * (MAX_MIME_BYTES + 16) + 4096;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Length of `bytes` once base64-encoded.
const fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// How long a result stays fresh: the script's hint up to `ceiling`, else the
/// config's `ttl`. A ceiling below the ttl never lowers it, and nothing
/// outlives `MAX_STALE`.
pub(super) fn effective_ttl(hint_secs: Option<u64>, ttl: Duration, ceiling: Duration) -> Duration {
    hint_secs.map_or(ttl, |hint| {
        Duration::from_secs(hint).min(ceiling.max(ttl).min(MAX_STALE))
    })
}

/// A snapshot file to write after a successful script run.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotTarget {
    pub path: PathBuf,
    pub ttl: Duration,
    pub keep: usize,
    /// The run's claim on the path, `None` for a run nothing supersedes.
    pub publication: Option<Publication>,
}

impl SnapshotTarget {
    /// Publish `prepared` unless a later run owns the list. A target with a
    /// publication checks the generation and renames under one lock, so an
    /// older result can never land on top of a newer snapshot.
    pub(crate) fn publish(&self, prepared: PreparedSnapshot) -> Result<()> {
        match &self.publication {
            Some(publication) => publication.publish(prepared),
            None => prepared.publish(),
        }
    }
}

/// One cached script output plus its metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub version: u32,
    pub fetched_at: u64,
    pub ttl_secs: u64,
    pub payload: String,
    /// The freshness the script asked for, as it asked. Clamping happens when
    /// the snapshot is read, so the current config always has the last word.
    #[serde(default)]
    pub hint_secs: Option<u64>,
}

impl Snapshot {
    pub fn age(&self) -> Duration {
        Duration::from_secs(now_secs().saturating_sub(self.fetched_at))
    }

    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.age() < ttl
    }

    /// Whether the snapshot is fresh for a config with freshness `ttl`: the
    /// script's own request counts, up to `ceiling`, so it can ask for less
    /// caching and, when the user allows it, for more.
    pub fn fresh_within(&self, ttl: Duration, ceiling: Duration) -> bool {
        self.age() < effective_ttl(self.hint_secs, ttl, ceiling)
    }

    pub fn is_usable(&self) -> bool {
        self.version == SNAPSHOT_VERSION && self.age() < MAX_STALE
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SourceStatus {
    pub source: String,
    pub last_refresh: Option<u64>,
    pub error: Option<String>,
}

/// Requests accepted by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
    Status,
    GetScriptFilter {
        key: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        query: String,
        #[serde(default)]
        force_refresh: bool,
        ttl_secs: u64,
        /// Run the script now and reply once its snapshot is written.
        #[serde(default)]
        wait: bool,
        /// Longest freshness the script may ask for; `ttl_secs` when missing.
        #[serde(default)]
        max_ttl_secs: Option<u64>,
    },
    GetScriptFilterPreview {
        key: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        query: String,
        #[serde(default)]
        force_refresh: bool,
        ttl_secs: u64,
    },
    RefreshSource {
        source: String,
        /// Reply once the source is warmed instead of right away.
        #[serde(default)]
        wait: bool,
    },
    /// Warm every enabled source, replying once all are done.
    RefreshAll,
    Clear,
    /// Put a `raffi` clipboard backend entry back on the clipboard.
    ClipboardSelect {
        id: String,
    },
    /// Put new content on the clipboard, recording it first when the policy
    /// allows. The command exits, so the server has to own the selection.
    ClipboardSet {
        formats: Vec<SetFormat>,
        /// Record the entry as a temporary one.
        #[serde(default)]
        temporary: bool,
        /// The content is a password-manager secret.
        #[serde(default)]
        secret: bool,
        /// Seconds after which the entry leaves the history and the
        /// clipboard is cleared, when it still holds the content.
        #[serde(default)]
        expires_in: Option<i64>,
        /// App to record as the source of the copy.
        #[serde(default)]
        source: Option<String>,
    },
    /// Show the launcher window for a client. Sent only to a server whose
    /// status reports the same `window` protocol. The server answers
    /// `Accepted` or `Refused`, then `Done` or `Toggled`.
    Open {
        /// The client's executable, so a server still running an older
        /// build after an upgrade refuses.
        build: String,
        /// The client's command line, its program name first.
        argv: Vec<String>,
        cwd: String,
        /// The client's environment, read and passed on to programs while
        /// its window shows.
        env: HashMap<String, String>,
    },
}

/// One MIME type of a `clipboard set` request, with its bytes base64-encoded:
/// the socket carries one JSON line per request, and a text entry can be 16
/// MiB of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SetFormat {
    pub mime: String,
    pub data: String,
}

/// Replies sent by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Response {
    Status {
        pid: u32,
        version: String,
        config_digest: String,
        sources: Vec<SourceStatus>,
        #[serde(default)]
        protocol: u32,
        /// The server records the clipboard history.
        #[serde(default)]
        clipboard: bool,
        /// Revision of the `Open` exchange when the server can show the
        /// launcher window, 0 when it cannot.
        #[serde(default)]
        window: u32,
    },
    /// The server shows the window for an `Open` request.
    Accepted,
    /// The server cannot show the window: the client shows it itself.
    Refused {
        reason: String,
    },
    /// The window of an accepted `Open` closed and what was picked ran.
    Done {
        #[serde(default)]
        error: Option<String>,
    },
    /// The `Open` request closed the window that was showing.
    Toggled,
    /// The content is on the clipboard. `id` is the history entry it was
    /// recorded as, absent when the policy kept it out of the history.
    ClipboardSet {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        expires_at: Option<i64>,
    },
    ScriptFilter {
        payload: Option<String>,
        stale: bool,
    },
    ScriptFilterPreview {
        payload: Option<String>,
        stale: bool,
    },
    Ok,
    Error {
        message: String,
        /// Stable reason, when the failure has one a client can act on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
    },
}

/// Stable reasons a clipboard request fails. A reply carries one so a client
/// does not have to read the message to know what went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    /// The content is larger than the limit of its type.
    TooLarge,
    /// The request, or a MIME type in it, does not make sense.
    InvalidArgument,
    /// The server does not record the clipboard.
    NotRecording,
    /// The entry left the history.
    NotFound,
    /// The database or the clipboard recorder failed.
    Io,
}

impl Response {
    /// A failure with nothing but a message to go on.
    fn failed(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            code: None,
        }
    }
}

/// A script run for the server. Output the script asked not to keep counts as
/// a failed refresh, reported with the first row's title.
fn server_run(result: Result<ScriptFilterResult, String>) -> Result<ScriptFilterResult, String> {
    match result {
        Ok(result) if result.not_stored => Err(result
            .items
            .first()
            .map(|item| super::ansi::strip_ansi(&item.title))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "the script asked not to cache this output".to_string())),
        other => other,
    }
}

impl From<Failure> for Response {
    fn from(failure: Failure) -> Self {
        Self::Error {
            message: failure.message,
            code: Some(failure.code),
        }
    }
}

/// A refused request: the message a client prints, and the code it branches
/// on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub code: ErrorCode,
    pub message: String,
}

impl Failure {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }
}

pub(crate) struct ServerState {
    pub config_digest: String,
    pub addons: AddonsConfig,
    status: Mutex<HashMap<String, SourceStatus>>,
    inflight: Mutex<HashSet<String>>,
    /// Lists a client is waiting on right now, counted: a refresh asked for
    /// by a user never waits behind a background one, so several runs of one
    /// list can overlap and each must be remembered until it ends.
    forced: Mutex<HashMap<String, usize>>,
    /// Publication generation of each list, bumped when a run a user waits on
    /// starts. A run remembers the generation it started under and drops its
    /// result once it has moved on, so a slow background run cannot overwrite
    /// a newer snapshot. Always locked after `forced`.
    generations: Mutex<HashMap<String, u64>>,
}

impl ServerState {
    pub fn new(addons: AddonsConfig) -> Self {
        Self {
            config_digest: addons_digest(&addons),
            addons,
            status: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            forced: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
        }
    }

    fn lock_generations(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.generations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn generation(&self, key: &str) -> u64 {
        self.lock_generations().get(key).copied().unwrap_or(0)
    }

    fn next_generation(&self, key: &str) -> u64 {
        let mut generations = self.lock_generations();
        let generation = generations.entry(key.to_string()).or_insert(0);
        *generation += 1;
        *generation
    }

    fn source_status(&self) -> Vec<SourceStatus> {
        let mut statuses: Vec<SourceStatus> = self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect();
        statuses.sort_by(|a, b| a.source.cmp(&b.source));
        statuses
    }

    fn set_status(&self, source: &str, error: Option<String>) {
        let mut statuses = self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = statuses
            .entry(source.to_string())
            .or_insert_with(|| SourceStatus {
                source: source.to_string(),
                last_refresh: None,
                error: None,
            });
        if error.is_none() {
            entry.last_refresh = Some(now_secs());
        }
        entry.error = error;
    }

    fn begin(&self, key: &str) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.to_string())
    }

    fn finish(&self, key: &str) {
        self.inflight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(key);
    }

    /// Mark a list as being refreshed until the guard is dropped, or `None`
    /// when another run, background or asked for by a user, already has it.
    fn hold(self: &Arc<Self>, key: String) -> Option<InFlight> {
        let forced = self
            .forced
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if forced.contains_key(&key) {
            return None;
        }
        let generation = self.generation(&key);
        self.begin(&key).then(|| InFlight {
            state: self.clone(),
            key,
            generation,
        })
    }

    /// Mark a list as being refreshed for a user who is waiting. Never
    /// refused, and it keeps background runs away until it is dropped.
    fn force(self: &Arc<Self>, key: String) -> Forced {
        let mut forced = self
            .forced
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *forced.entry(key.clone()).or_insert(0) += 1;
        let generation = self.next_generation(&key);
        drop(forced);
        Forced {
            state: self.clone(),
            key,
            generation,
        }
    }
}

/// A run's claim on a list snapshot: it may write the file only while the
/// generation it started under is still the newest for that list.
#[derive(Clone)]
pub(crate) struct Publication {
    state: Arc<ServerState>,
    key: String,
    generation: u64,
}

impl Publication {
    fn is_current(&self) -> bool {
        self.state.generation(&self.key) == self.generation
    }

    /// Publish `prepared` only while the generation this run started under is
    /// still the newest for the list. The check and the rename hold the lock
    /// that advancing a generation takes, so a run started later either
    /// publishes before this one is refused or after it renamed: the newest
    /// result always ends up in the file.
    fn publish(&self, prepared: PreparedSnapshot) -> Result<()> {
        let generations = self.state.lock_generations();
        if generations.get(&self.key).copied().unwrap_or(0) != self.generation {
            drop(generations);
            prepared.discard();
            return Ok(());
        }
        prepared.publish()
    }
}

impl std::fmt::Debug for Publication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Publication({}, {})", self.key, self.generation)
    }
}

/// Releases a list marked by `ServerState::force`.
struct Forced {
    state: Arc<ServerState>,
    key: String,
    generation: u64,
}

impl Forced {
    /// The claim this run publishes under: a later run for the same list
    /// bumps the generation and takes the snapshot over.
    fn publication(&self) -> Publication {
        Publication {
            state: self.state.clone(),
            key: self.key.clone(),
            generation: self.generation,
        }
    }
}

impl Drop for Forced {
    fn drop(&mut self) {
        let mut forced = self
            .state
            .forced
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(count) = forced.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                forced.remove(&self.key);
            }
        }
    }
}

/// Releases a list marked by `ServerState::hold`, whatever ends the run.
struct InFlight {
    state: Arc<ServerState>,
    key: String,
    generation: u64,
}

impl InFlight {
    /// The claim this background run publishes under.
    fn publication(&self) -> Publication {
        Publication {
            state: self.state.clone(),
            key: self.key.clone(),
            generation: self.generation,
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.state.finish(&self.key);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// FNV-1a, used for stable snapshot keys and config digests.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub(crate) fn stable_hash(value: &str) -> String {
    format!("{:016x}", fnv1a(value.as_bytes()))
}

pub(crate) fn config_digest<T: Serialize>(value: &T) -> String {
    // Going through `Value` sorts map keys, so a `HashMap` such as a Script
    // Filter's `env` hashes the same in every process.
    serde_json::to_value(value)
        .and_then(|value| serde_json::to_string(&value))
        .map(|json| stable_hash(&json))
        .unwrap_or_else(|_| stable_hash("unserializable"))
}

/// Digest of the addon settings the server acts on, so the UI can
/// tell when a running server was started with another configuration.
pub(crate) fn addons_digest(addons: &AddonsConfig) -> String {
    let script_filters = config_digest(&addons.script_filters);
    let browser_history = config_digest(&addons.browser_history);
    let slack = config_digest(&addons.slack);
    let clipboard = &addons.clipboard;
    let clipboard = format!(
        "{:?}:{}:{}:{}:{}:{}:{}:{}",
        clipboard.backend,
        clipboard.max_items(),
        clipboard.max_size_bytes(),
        clipboard.max_image_size_bytes(),
        config_digest(&clipboard.exclude_patterns),
        config_digest(&clipboard.temporary_patterns),
        clipboard.temporary_timeout(),
        clipboard.temporary_password_manager(),
    );
    stable_hash(&format!(
        "{script_filters}:{browser_history}:{slack}:{clipboard}"
    ))
}

pub(super) fn cache_root() -> Result<PathBuf> {
    let mut path = crate::ui::get_mru_cache_path()?;
    path.pop();
    Ok(path)
}

fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    Ok(())
}

pub(crate) fn script_filter_cache_dir() -> Result<PathBuf> {
    let path = cache_root()?.join("script-filters");
    private_dir(&path)?;
    Ok(path)
}

pub(crate) fn browser_history_cache_dir() -> Result<PathBuf> {
    let path = cache_root()?.join("browser-history");
    private_dir(&path)?;
    Ok(path)
}

/// `$XDG_RUNTIME_DIR/raffi/cache.sock`, or a private fallback under `/tmp`.
pub(crate) fn cache_socket_path() -> Result<PathBuf> {
    if cfg!(test) {
        anyhow::bail!("the cache socket is disabled in tests");
    }
    let base = crate::process::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/raffi-{}", unsafe { libc::getuid() })));
    let dir = base.join("raffi");
    private_dir(&dir)?;
    Ok(dir.join("cache.sock"))
}

fn list_path(key: &str) -> Result<PathBuf> {
    Ok(script_filter_cache_dir()?
        .join("lists")
        .join(format!("{key}.json")))
}

fn preview_path(key: &str) -> Result<PathBuf> {
    Ok(script_filter_cache_dir()?
        .join("previews")
        .join(format!("{key}.json")))
}

pub(crate) fn script_filter_list_key(config: &ScriptFilterConfig, query: &str) -> String {
    let mut env: Vec<(&String, &String)> = config.env.iter().collect();
    env.sort();
    let mut input = format!(
        "list\0{}\0{}\0{}",
        config.command,
        config.args.join("\0"),
        query
    );
    for (name, value) in env {
        input.push('\0');
        input.push_str(name);
        input.push('=');
        input.push_str(value);
    }
    stable_hash(&input)
}

pub(crate) fn script_filter_preview_key(request: &PreviewRequest) -> String {
    let mut env: Vec<(&String, &String)> = request.env.iter().collect();
    env.sort();
    let mut input = format!(
        "preview\0{}\0{}\0{}",
        request.command,
        request.args.join("\0"),
        request.query
    );
    for (name, value) in env {
        input.push('\0');
        input.push_str(name);
        input.push('=');
        input.push_str(value);
    }
    stable_hash(&input)
}

pub(crate) fn list_target(
    config: &ScriptFilterConfig,
    query: &str,
    ttl: Duration,
    publication: Option<Publication>,
) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: list_path(&script_filter_list_key(config, query)).ok()?,
        ttl,
        keep: LIST_KEEP,
        publication,
    })
}

pub(crate) fn preview_target(request: &PreviewRequest, ttl: Duration) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: preview_path(&script_filter_preview_key(request)).ok()?,
        ttl,
        keep: PREVIEW_KEEP,
        publication: None,
    })
}

pub(crate) fn read_list(key: &str) -> Option<Snapshot> {
    read_snapshot_path(&list_path(key).ok()?)
}

pub(super) type SnapshotStamp = (u64, u64, i64, i64);

pub(super) enum SnapshotUpdate {
    Missing,
    Unchanged,
    Changed(SnapshotStamp, Option<Snapshot>),
}

fn read_warm_snapshot_path(path: &Path, known: Option<SnapshotStamp>) -> Option<SnapshotUpdate> {
    let stamp = |meta: fs::Metadata| (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec());
    if Some(stamp(fs::metadata(path).ok()?)) == known {
        return Some(SnapshotUpdate::Unchanged);
    }
    // Match the stamp to the open file in case a writer replaced the path
    let mut file = fs::File::open(path).ok()?;
    let actual = stamp(file.metadata().ok()?);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let snapshot = serde_json::from_slice::<Snapshot>(&bytes)
        .ok()
        .filter(|snapshot| snapshot.version == SNAPSHOT_VERSION);
    Some(SnapshotUpdate::Changed(actual, snapshot))
}

pub(crate) fn read_preview(key: &str) -> Option<Snapshot> {
    read_snapshot_path(&preview_path(key).ok()?)
}

pub(crate) fn read_snapshot_path(path: &Path) -> Option<Snapshot> {
    let bytes = fs::read(path).ok()?;
    let snapshot = serde_json::from_slice::<Snapshot>(&bytes).ok()?;
    snapshot.is_usable().then_some(snapshot)
}

/// Read a private JSON file, ignoring malformed or missing ones.
pub(crate) fn read_private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Write a private JSON file atomically with mode `0600`.
pub(crate) fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    write_private_bytes(path, &bytes)
}

/// Write `bytes` to `path` atomically, in a `0600` file: an older file's
/// wider permissions go with the rename.
pub(crate) fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = write_private_temp(path, bytes)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

/// Write `bytes` to a private `0600` temporary file next to `path`, and return
/// its name: the caller renames it into place.
fn write_private_temp(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    private_dir(path.parent().context("cache file has no parent")?)?;
    let temp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    Ok(temp)
}

/// A snapshot serialized into a private temporary file, waiting for the
/// rename that makes it visible at `path`.
pub(crate) struct PreparedSnapshot {
    path: PathBuf,
    temp: PathBuf,
    keep: usize,
}

impl PreparedSnapshot {
    /// Rename the file into place and prune the directory. A caller that has
    /// to keep a later write from being overtaken does this while holding the
    /// lock that decides which run owns the list.
    fn publish(self) -> Result<()> {
        if let Err(error) = fs::rename(&self.temp, &self.path) {
            let _ = fs::remove_file(&self.temp);
            return Err(error.into());
        }
        if let Some(dir) = self.path.parent() {
            prune_snapshots(dir, self.keep);
        }
        Ok(())
    }

    /// Drop the prepared file, leaving `path` as it was.
    fn discard(self) {
        let _ = fs::remove_file(&self.temp);
    }
}

/// Serialize a snapshot into a private temporary file next to `path`. The
/// write and its fsync happen here, outside any lock; only the rename that
/// publishes it is short enough to hold one.
pub(crate) fn prepare_snapshot_path(
    path: &Path,
    payload: &str,
    ttl: Duration,
    keep: usize,
    hint_secs: Option<u64>,
) -> Result<PreparedSnapshot> {
    let snapshot = Snapshot {
        version: SNAPSHOT_VERSION,
        fetched_at: now_secs(),
        ttl_secs: ttl.as_secs(),
        payload: payload.to_string(),
        hint_secs,
    };
    let bytes = serde_json::to_vec(&snapshot)?;
    Ok(PreparedSnapshot {
        path: path.to_path_buf(),
        temp: write_private_temp(path, &bytes)?,
        keep,
    })
}

/// Write a snapshot at once, for a caller whose writes nothing supersedes.
pub(crate) fn write_snapshot_path(
    path: &Path,
    payload: &str,
    ttl: Duration,
    keep: usize,
    hint_secs: Option<u64>,
) -> Result<()> {
    prepare_snapshot_path(path, payload, ttl, keep, hint_secs)?.publish()
}

fn prune_snapshots(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect();
    files.retain(|(modified, path)| {
        let old = now
            .duration_since(*modified)
            .is_ok_and(|age| age > MAX_STALE);
        if old {
            let _ = fs::remove_file(path);
        }
        !old
    });
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let remove = files.len() - keep;
    for (_, path) in files.into_iter().take(remove) {
        let _ = fs::remove_file(path);
    }
}

/// Remove every script-filter and browser-history snapshot.
pub(crate) fn clear_snapshots() {
    if let Ok(root) = cache_root() {
        for name in ["script-filters", "browser-history"] {
            let _ = fs::remove_dir_all(root.join(name));
        }
    }
}

/// Ask a running server to clear its state and snapshots. Best effort.
pub(crate) fn notify_clear() {
    let _ = try_request(&Request::Clear, SERVER_CLIENT_TIMEOUT);
}

/// Best-effort request to a server; a missing or slow server is ignored.
pub(crate) fn try_request(request: &Request, timeout: Duration) -> Option<Response> {
    call(request, timeout).ok()
}

/// Why a request to the server got no usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallError {
    /// No server listens on the socket.
    Unreachable,
    /// The server failed the request, or did not answer in time.
    Failed(String),
}

fn call(request: &Request, timeout: Duration) -> Result<Response, CallError> {
    let path = cache_socket_path().map_err(|_| CallError::Unreachable)?;
    let mut stream = UnixStream::connect(path).map_err(|_| CallError::Unreachable)?;
    let failed = |error: std::io::Error| CallError::Failed(error.to_string());
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let mut line =
        serde_json::to_vec(request).map_err(|error| CallError::Failed(error.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(failed)?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).map_err(failed)?;
    if reply.is_empty() {
        return Err(CallError::Failed(
            "the Raffi server closed the connection".into(),
        ));
    }
    serde_json::from_str(&reply).map_err(|error| CallError::Failed(error.to_string()))
}

/// What the UI needs to know about a running server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerInfo {
    pub protocol: u32,
    pub config_digest: String,
}

/// Ask whether a server is running. Blocking: run it off the UI thread.
pub(crate) fn probe() -> Option<ServerInfo> {
    match try_request(&Request::Status, PROBE_TIMEOUT)? {
        Response::Status {
            protocol,
            config_digest,
            ..
        } => Some(ServerInfo {
            protocol,
            config_digest,
        }),
        _ => None,
    }
}

/// Ask the server to put the `raffi` backend clipboard entry `id`
/// back on the clipboard. Blocking: run it off the UI thread.
pub(crate) fn clipboard_select(id: &str) -> Result<(), String> {
    match call(
        &Request::ClipboardSelect { id: id.to_string() },
        CLIPBOARD_TIMEOUT,
    ) {
        Ok(Response::Error { message, .. }) => Err(message),
        Ok(_) => Ok(()),
        Err(CallError::Unreachable) => Err(CLIPBOARD_NOT_RUNNING.to_string()),
        Err(CallError::Failed(message)) => Err(message),
    }
}

/// Whether a running server records the clipboard history.
/// Blocking: run it off the UI thread.
pub(crate) fn clipboard_recording() -> bool {
    clipboard_report().is_some_and(|(recording, _)| recording)
}

/// Whether a running server records the clipboard history, and the
/// protocol revision it speaks. Blocking: run it off the UI thread.
pub(crate) fn clipboard_report() -> Option<(bool, u32)> {
    match try_request(&Request::Status, PROBE_TIMEOUT)? {
        Response::Status {
            clipboard,
            protocol,
            ..
        } => Some((clipboard, protocol)),
        _ => None,
    }
}

/// The history entry a `clipboard set` was recorded as, with the deadline it
/// leaves the history at. Both are absent when the policy kept the content
/// out of the history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SetOutcome {
    pub id: Option<String>,
    pub expires_at: Option<i64>,
}

/// Why a `clipboard set` failed, so the command line can print the stable
/// code a script branches on instead of reading the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetFailure {
    /// The server speaks an older protocol and cannot set the clipboard.
    ServerOutdated(u32),
    /// No server listens on the socket.
    NotRunning,
    /// The server, or the socket, failed: its code when it gave one.
    Failed(Option<ErrorCode>, String),
}

impl std::fmt::Display for SetFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerOutdated(protocol) => write!(
                formatter,
                "the running Raffi server is from an older Raffi and cannot set the \
                 clipboard; restart `raffi --server` (it speaks protocol \
                 {protocol}, this raffi {PROTOCOL})"
            ),
            Self::NotRunning => formatter.write_str(CLIPBOARD_NOT_RUNNING),
            Self::Failed(_, message) => formatter.write_str(message),
        }
    }
}

/// Ask the server to put `formats` on the clipboard. The server
/// records them in the history first when its policy allows, and owns the
/// selection afterwards, since this process exits. Blocking: run it off the
/// UI thread.
pub(crate) fn clipboard_set(
    formats: &[(String, Vec<u8>)],
    temporary: bool,
    secret: bool,
    expires_in: Option<i64>,
    source: Option<String>,
) -> Result<SetOutcome, SetFailure> {
    match probe() {
        Some(info) if info.protocol < CLIPBOARD_SET_PROTOCOL => {
            return Err(SetFailure::ServerOutdated(info.protocol))
        }
        Some(_) => {}
        None => return Err(SetFailure::NotRunning),
    }
    let request = Request::ClipboardSet {
        formats: encode_formats(formats),
        temporary,
        secret,
        expires_in,
        source,
    };
    match call(&request, CLIPBOARD_TIMEOUT) {
        Ok(Response::ClipboardSet { id, expires_at }) => Ok(SetOutcome { id, expires_at }),
        Ok(Response::Error { message, code }) => Err(SetFailure::Failed(code, message)),
        Ok(_) => Err(SetFailure::Failed(
            None,
            "the Raffi server sent an unexpected reply".into(),
        )),
        Err(CallError::Unreachable) => Err(SetFailure::NotRunning),
        Err(CallError::Failed(message)) => Err(SetFailure::Failed(None, message)),
    }
}

fn encode_formats(formats: &[(String, Vec<u8>)]) -> Vec<SetFormat> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    formats
        .iter()
        .map(|(mime, data)| SetFormat {
            mime: mime.clone(),
            data: engine.encode(data),
        })
        .collect()
}

fn decode_formats(formats: &[SetFormat]) -> Result<Vec<(String, Vec<u8>)>, String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    formats
        .iter()
        .map(|format| {
            engine
                .decode(&format.data)
                .map(|data| (format.mime.clone(), data))
                .map_err(|error| format!("cannot read the `{}` content: {error}", format.mime))
        })
        .collect()
}

/// Whether a server listens on the socket.
pub(crate) fn listening() -> bool {
    cache_socket_path().is_ok_and(|path| UnixStream::connect(path).is_ok())
}

#[derive(Debug, Clone)]
pub(crate) struct RefreshOutcome {
    pub result: Result<(), CallError>,
    pub shown_refreshed: Option<bool>,
}

/// Run the shown list's request first, retaining its outcome if Refresh All
/// fails on a different source. Blocking: run it off the UI thread.
pub(crate) fn refresh(shown: Option<Request>, all: bool) -> RefreshOutcome {
    let request = |request: &Request| match call(request, WAIT_TIMEOUT)? {
        Response::Ok | Response::ScriptFilter { .. } => Ok(()),
        Response::Error { message, .. } => Err(CallError::Failed(message)),
        _ => Err(CallError::Failed(
            "the Raffi server sent an unexpected reply".into(),
        )),
    };
    let shown = shown.as_ref().map(request);
    let shown_refreshed = shown.as_ref().map(Result::is_ok);
    let result = shown.unwrap_or(Ok(()));
    let result = if all && result != Err(CallError::Unreachable) {
        match (result, request(&Request::RefreshAll)) {
            (Err(CallError::Failed(shown)), Err(CallError::Failed(all))) => {
                Err(CallError::Failed(format!("{shown}; {all}")))
            }
            (_, Err(CallError::Unreachable)) => Err(CallError::Unreachable),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    } else {
        result
    };
    RefreshOutcome {
        result,
        shown_refreshed,
    }
}

/// Waited request re-running the script filter `config` for `query`.
pub(crate) fn script_filter_refresh_request(
    config: &ScriptFilterConfig,
    query: &str,
) -> Option<Request> {
    let ttl = config.cache_ttl()?;
    let mut env = config.env.clone();
    env.insert("RAFFI_REFRESH".into(), "1".into());
    Some(Request::GetScriptFilter {
        key: script_filter_list_key(config, query),
        command: config.command.clone(),
        args: config.args.clone(),
        env,
        query: query.to_string(),
        force_refresh: true,
        ttl_secs: ttl.as_secs(),
        wait: true,
        max_ttl_secs: config.cache_ceiling().map(|ceiling| ceiling.as_secs()),
    })
}

/// Handle one request. Public within the crate so protocol tests can drive it
/// over a temporary listener.
pub(crate) fn handle_request(state: &Arc<ServerState>, request: Request) -> Response {
    match request {
        Request::Status => Response::Status {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            config_digest: state.config_digest.clone(),
            sources: state.source_status(),
            protocol: PROTOCOL,
            clipboard: super::clipboard_watch::recording(),
            window: super::resident::window_protocol(),
        },
        Request::Open { .. } => Response::Refused {
            reason: "the window is opened over a connection of its own".into(),
        },
        Request::GetScriptFilter {
            key,
            command,
            args,
            env,
            query,
            ttl_secs,
            wait: true,
            ..
        } => {
            let ttl = Duration::from_secs(ttl_secs);
            let name = Path::new(&command).file_name().map_or_else(
                || command.clone(),
                |name| name.to_string_lossy().into_owned(),
            );
            // An F5 never waits for a background run, so it is registered
            // whether or not one is going on, and keeps the next one away.
            let forced = state.force(format!("list:{key}"));
            let publication = forced.publication();
            let result = logged(&format!("script filter {name} {query:?}"), || {
                server_run(script_filters::run_script_filter(
                    &command,
                    &args,
                    &env,
                    &query,
                    None,
                    list_target_from_key(&key, ttl, Some(publication)),
                    Some(SERVER_RUN_TIMEOUT),
                ))
            });
            state.set_status("script-filters", result.as_ref().err().cloned());
            match result {
                Ok(_) => Response::ScriptFilter {
                    payload: read_list(&key).map(|snapshot| snapshot.payload),
                    stale: false,
                },
                Err(message) => Response::failed(message),
            }
        }
        Request::GetScriptFilter {
            key,
            command,
            args,
            env,
            query,
            force_refresh,
            ttl_secs,
            wait: false,
            max_ttl_secs,
        } => {
            let ttl = Duration::from_secs(ttl_secs);
            let ceiling = max_ttl_secs.map_or(ttl, Duration::from_secs);
            let snapshot = read_list(&key);
            let stale = force_refresh
                || snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !snapshot.fresh_within(ttl, ceiling));
            let payload = snapshot.map(|snapshot| snapshot.payload);
            let held = (payload.is_none() || stale)
                .then(|| state.hold(format!("list:{key}")))
                .flatten();
            if let Some(held) = held {
                let publication = held.publication();
                let claim = publication.clone();
                let state = state.clone();
                std::thread::spawn(crate::process::inherit(move || {
                    crate::process::mark_server_thread();
                    let result = server_run(script_filters::run_script_filter(
                        &command,
                        &args,
                        &env,
                        &query,
                        None,
                        list_target_from_key(&key, ttl, Some(publication)),
                        Some(SERVER_RUN_TIMEOUT),
                    ));
                    // A run a user's refresh took over says nothing about the
                    // list: it neither published nor is the latest word on it.
                    if claim.is_current() {
                        state.set_status("script-filters", result.err());
                    }
                    drop(held);
                }));
            }
            Response::ScriptFilter { payload, stale }
        }
        Request::GetScriptFilterPreview {
            key,
            command,
            args,
            env,
            query,
            force_refresh,
            ttl_secs,
        } => {
            let ttl = Duration::from_secs(ttl_secs);
            let snapshot = read_preview(&key);
            let stale = force_refresh
                || snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !snapshot.is_fresh(ttl));
            let payload = snapshot.map(|snapshot| snapshot.payload);
            if (payload.is_none() || stale) && state.begin(&format!("preview:{key}")) {
                let state = state.clone();
                let job_key = format!("preview:{key}");
                std::thread::spawn(crate::process::inherit(move || {
                    crate::process::mark_server_thread();
                    let request = PreviewRequest {
                        command,
                        args,
                        query,
                        env,
                        cache: preview_target_from_key(&key, ttl),
                    };
                    let error = request.run().err().map(|error| error.into_message());
                    state.set_status("script-filters", error);
                    state.finish(&job_key);
                }));
            }
            Response::ScriptFilterPreview { payload, stale }
        }
        Request::RefreshSource { source, wait: true } => {
            match logged(&source, || warm_named(state, &source)) {
                Ok(()) => Response::Ok,
                Err(message) => Response::failed(message),
            }
        }
        Request::RefreshSource {
            source,
            wait: false,
        } => {
            let state = state.clone();
            std::thread::spawn(crate::process::inherit(move || {
                crate::process::mark_server_thread();
                let _ = logged(&source, || warm_named(&state, &source));
            }));
            Response::Ok
        }
        Request::RefreshAll => match logged("all sources", || warm_all(state)) {
            Ok(()) => Response::Ok,
            Err(message) => Response::failed(message),
        },
        Request::Clear => {
            clear_snapshots();
            super::slack_api::clear_cache();
            {
                let mut statuses = state
                    .status
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                statuses.clear();
            }
            Response::Ok
        }
        Request::ClipboardSelect { id } => match super::clipboard_watch::select(&id) {
            Ok(()) => Response::Ok,
            Err(message) => Response::failed(message),
        },
        Request::ClipboardSet {
            formats,
            temporary,
            secret,
            expires_in,
            source,
        } => match serve_set(
            state,
            &formats,
            temporary,
            secret,
            expires_in,
            source.as_deref(),
        ) {
            Ok((id, expires_at)) => Response::ClipboardSet { id, expires_at },
            Err(failure) => failure.into(),
        },
    }
}

/// Put the content of a `clipboard set` on the clipboard, recording it in the
/// history first when the policy allows, and report the entry it landed in.
fn serve_set(
    state: &Arc<ServerState>,
    formats: &[SetFormat],
    temporary: bool,
    secret: bool,
    expires_in: Option<i64>,
    source: Option<&str>,
) -> Result<(Option<String>, Option<i64>), Failure> {
    let config = &state.addons.clipboard;
    let limits = super::clipboard_store::Limits::from_config(config);
    // The request is checked before the server says whether it records at
    // all: a malformed request is refused whatever the state of the server.
    check_set_request(formats, &limits)?;
    if let Some(secs) = expires_in {
        let problem = match u64::try_from(secs) {
            Ok(secs) => crate::temporary_timeout_problem(secs),
            Err(_) => Some("must be greater than zero".to_string()),
        };
        if let Some(problem) = problem {
            return Err(Failure::invalid(format!("expires_in: {problem}")));
        }
    }
    let source = source
        .map(super::clipboard_store::validate_source)
        .transpose()
        .map_err(Failure::invalid)?;
    if !super::clipboard_watch::recording() {
        return Err(Failure::new(
            ErrorCode::NotRecording,
            "the Raffi server is not recording the clipboard; \
             set addons.clipboard.backend to raffi or auto and restart it",
        ));
    }
    let policy = super::clipboard_policy::Policy::from_config(config).map_err(Failure::invalid)?;
    let formats = decode_formats(formats).map_err(Failure::invalid)?;
    let mut store = super::clipboard_store::Store::open_default()
        .map_err(|message| Failure::new(ErrorCode::Io, message))?;
    store.enable_ocr(config.ocr.enabled);
    let now = super::clipboard_store::now_secs();
    let served = record_set(
        &mut store, formats, temporary, secret, expires_in, source, &limits, &policy, now,
    )?;
    // Only an explicit lifetime takes the content off the clipboard, recorded
    // or not.
    let clear_at = expires_in.map(|secs| now.saturating_add(secs));
    super::clipboard_watch::set(served.id.clone(), served.formats, clear_at)
        .map_err(|message| Failure::new(ErrorCode::Io, message))?;
    Ok((served.id, served.expires_at))
}

/// Check the framing of a `clipboard set` request, and the size of its
/// encoded content, before any of it is decoded: the server must not
/// allocate what it is about to refuse.
fn check_set_request(
    formats: &[SetFormat],
    limits: &super::clipboard_store::Limits,
) -> Result<(), Failure> {
    if formats.is_empty() {
        return Err(Failure::invalid("the request holds no content to set"));
    }
    if formats.len() > MAX_SET_FORMATS {
        return Err(Failure::new(
            ErrorCode::TooLarge,
            format!(
                "the request holds {} formats, more than the {MAX_SET_FORMATS} \
                 the server reads",
                formats.len()
            ),
        ));
    }
    let mut total = 0u64;
    for format in formats {
        if format.mime.len() > MAX_MIME_BYTES {
            return Err(Failure::new(
                ErrorCode::InvalidArgument,
                format!(
                    "a MIME type of {} bytes, more than the {MAX_MIME_BYTES} the \
                     server reads",
                    format.mime.len()
                ),
            ));
        }
        super::clipboard_store::validate_mime(&format.mime).map_err(|reason| {
            Failure::invalid(format!("the request carries a MIME type: {reason}"))
        })?;
        let limit = format_limit(&format.mime, limits);
        // Base64 is 4 bytes of line for every 3 of content, so the encoded
        // length alone already bounds the decoded one for all but the last
        // few bytes.
        if format.data.len() as u64 > base64_len(limit as usize) as u64 {
            return Err(too_large(
                &format.mime,
                format.data.len() as u64 / 4 * 3,
                limit,
            ));
        }
        let decoded = decoded_len(&format.data);
        if decoded > limit {
            return Err(too_large(&format.mime, decoded, limit));
        }
        total = total.saturating_add(decoded);
    }
    if total > limits.max_size {
        return Err(Failure::new(
            ErrorCode::TooLarge,
            format!(
                "the request holds {total} bytes, over the {} byte limit of the history",
                limits.max_size
            ),
        ));
    }
    Ok(())
}

fn too_large(mime: &str, bytes: u64, limit: u64) -> Failure {
    Failure::new(
        ErrorCode::TooLarge,
        format!("{mime} is {bytes} bytes, over the {limit} byte limit"),
    )
}

/// The size a format of `mime` may reach: what the store records, capped so
/// that a request can never carry more than the server reads.
fn format_limit(mime: &str, limits: &super::clipboard_store::Limits) -> u64 {
    if mime.starts_with("image/") {
        limits.max_image_size.min(MAX_SET_FORMAT_BYTES)
    } else {
        super::clipboard_store::MAX_TEXT_BYTES as u64
    }
}

/// How many bytes `encoded` base64 holds, not counting its padding.
fn decoded_len(encoded: &str) -> u64 {
    let padding = encoded
        .bytes()
        .rev()
        .take(2)
        .take_while(|&byte| byte == b'=')
        .count();
    (encoded.len() as u64 / 4 * 3).saturating_sub(padding as u64)
}

/// What a `clipboard set` leaves on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    /// The formats the selection offers.
    formats: Vec<(String, Vec<u8>)>,
    /// The history entry they were recorded as, absent when the policy kept
    /// them out of it.
    id: Option<String>,
    /// When that entry leaves the history.
    expires_at: Option<i64>,
}

/// Record the content of a `clipboard set` in `store`, and report the formats
/// to serve, the entry they were recorded as, and its deadline. Content the
/// policy keeps out of the history is still served, with the password-manager
/// hint when it is a secret. `expires_in` overrides the configured timeout,
/// and records a secret even when the password-manager opt-in is off.
#[allow(clippy::too_many_arguments)]
fn record_set(
    store: &mut super::clipboard_store::Store,
    formats: Vec<(String, Vec<u8>)>,
    temporary: bool,
    secret: bool,
    expires_in: Option<i64>,
    source: Option<String>,
    limits: &super::clipboard_store::Limits,
    policy: &super::clipboard_policy::Policy,
    now: i64,
) -> Result<Served, Failure> {
    let opted_in;
    let policy = if secret && expires_in.is_some() && !policy.temporary_password_manager {
        opted_in = super::clipboard_policy::Policy {
            temporary_password_manager: true,
            ..policy.clone()
        };
        &opted_in
    } else {
        policy
    };
    // The policy is applied before the store takes the content, so what it
    // keeps out of the history is served again without copying it.
    let text = super::clipboard_store::text_of(&formats);
    if policy.disposition(&text, secret) == super::clipboard_policy::Disposition::Skip {
        return Ok(Served {
            formats: with_hint(formats, secret),
            id: None,
            expires_at: None,
        });
    }
    let capture = super::clipboard_store::Capture {
        formats,
        copied_at: now,
        source,
        password_marked: secret,
    };
    let lifetime = expires_in.or(temporary.then_some(policy.temporary_timeout));
    let inserted = match lifetime {
        Some(lifetime) if expires_in.is_some() => {
            store.insert_expiring(capture, lifetime, limits, policy, now)
        }
        Some(lifetime) => store.insert_temporary(capture, lifetime, limits, policy, now),
        None => store.insert_filtered(capture, limits, policy, now),
    };
    match inserted {
        Ok(hash) => {
            let expires_at = store
                .deadline(&hash)
                .map_err(|message| Failure::new(ErrorCode::Io, message))?;
            let stored = store
                .formats(&hash, now)
                .map_err(|message| Failure::new(ErrorCode::Io, message))?;
            Ok(Served {
                formats: stored,
                id: Some(hash),
                expires_at,
            })
        }
        Err(super::clipboard_store::Skipped::TooLarge(reason)) => Err(Failure::new(
            ErrorCode::TooLarge,
            format!("too large: {reason}"),
        )),
        Err(skipped) => Err(Failure::new(ErrorCode::Io, skipped.to_string())),
    }
}

/// `formats` plus the password-manager hint, so a secret nobody records is
/// still skipped by the other clipboard managers.
fn with_hint(mut formats: Vec<(String, Vec<u8>)>, secret: bool) -> Vec<(String, Vec<u8>)> {
    if secret {
        formats.push((
            super::clipboard_policy::PASSWORD_HINT.to_string(),
            super::clipboard_store::PASSWORD_MARKER.to_vec(),
        ));
    }
    formats
}

/// Runs a refresh someone asked for, logging when it starts and how it ended.
fn logged<T>(what: &str, refresh: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    eprintln!("raffi: refreshing {what}");
    let started = Instant::now();
    let result = refresh();
    let elapsed = started.elapsed().as_secs_f32();
    match &result {
        Ok(_) => eprintln!("raffi: refreshed {what} in {elapsed:.1}s"),
        Err(message) => eprintln!("raffi: refreshing {what} failed after {elapsed:.1}s: {message}"),
    }
    result
}

fn list_target_from_key(
    key: &str,
    ttl: Duration,
    publication: Option<Publication>,
) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: list_path(key).ok()?,
        ttl,
        keep: LIST_KEEP,
        publication,
    })
}

fn preview_target_from_key(key: &str, ttl: Duration) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: preview_path(key).ok()?,
        ttl,
        keep: PREVIEW_KEEP,
        publication: None,
    })
}

/// What one connection carried.
enum Incoming {
    Request(Request),
    /// A line the server read but cannot serve.
    Refused(Failure),
    /// Nothing to answer: the peer closed the connection.
    Closed,
}

/// Read one request line, refusing anything longer than `limit` bytes before
/// the rest of it is read.
fn read_request(stream: &mut UnixStream, limit: usize) -> Incoming {
    let mut reader = BufReader::new(stream).take(limit as u64 + 1);
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).is_err() || line.is_empty() {
        return Incoming::Closed;
    }
    if line.len() > limit {
        return Incoming::Refused(Failure::new(
            ErrorCode::TooLarge,
            format!("the request is longer than the {limit} bytes the server reads"),
        ));
    }
    match serde_json::from_slice(&line) {
        Ok(request) => Incoming::Request(request),
        Err(error) => Incoming::Refused(Failure::invalid(format!(
            "cannot read the request: {error}"
        ))),
    }
}

fn serve_connection(state: Arc<ServerState>, mut stream: UnixStream) {
    crate::process::mark_server_thread();
    let reply = match read_request(&mut stream, MAX_REQUEST_LINE) {
        Incoming::Request(Request::Open {
            build,
            argv,
            cwd,
            env,
        }) => {
            return super::resident::serve_open(stream, &build, &argv, &cwd, env);
        }
        Incoming::Request(request) => handle_request(&state, request),
        Incoming::Refused(failure) => failure.into(),
        Incoming::Closed => return,
    };
    if let Ok(mut line) = serde_json::to_vec(&reply) {
        line.push(b'\n');
        let _ = stream.write_all(&line);
    }
}

/// Serve requests on `listener` until the process exits.
pub(crate) fn serve(listener: UnixListener, state: Arc<ServerState>) -> Result<()> {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(crate::process::inherit(move || {
                    serve_connection(state, stream)
                }));
            }
            Err(error) => crate::debug_log!("cache: accept failed: {error}"),
        }
    }
    Ok(())
}

/// Run the server in the foreground. With a display, it also shows the
/// launcher window for clients: `window` holds the settings it was started
/// with.
pub(crate) fn run_server(config: ParsedConfig, window: crate::ui::UISettings) -> Result<()> {
    // Before any thread starts: it may set the environment.
    let host = super::resident::prepare(window);
    let socket = cache_socket_path()?;
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
    let state = Arc::new(ServerState::new(config.addons));
    eprintln!("raffi: server listening on {}", socket.display());
    super::hot_files::keep_warm();
    let clipboard = &state.addons.clipboard;
    if clipboard.enabled != Some(false)
        && super::clipboard_history::server_records(
            clipboard.backend,
            super::clipboard_history::running_cmdlines,
        )
    {
        let policy =
            super::clipboard_policy::Policy::from_config(clipboard).map_err(anyhow::Error::msg)?;
        super::clipboard_store::start_sweeper();
        if clipboard.ocr.enabled {
            if let Some((_, problem)) = clipboard
                .clipboard_problems()
                .into_iter()
                .find(|(key, _)| key == "/addons/clipboard/ocr/languages")
            {
                eprintln!(
                    "raffi: clipboard OCR disabled: addons.clipboard.ocr.languages: {problem}"
                );
            } else {
                super::clipboard_ocr::start(clipboard.ocr.languages.join("+"));
            }
        }
        super::clipboard_watch::start(
            super::clipboard_store::Limits::from_config(clipboard),
            policy,
            clipboard.ocr.enabled,
        );
    }
    warm_loop(state.clone());
    let Some(host) = host else {
        return serve(listener, state);
    };
    let server = std::thread::spawn(crate::process::inherit(move || serve(listener, state)));
    super::resident::run(host);
    server
        .join()
        .map_err(|_| anyhow::anyhow!("the server thread panicked"))?
}

/// Start periodic warming for every source that opted in.
fn warm_loop(state: Arc<ServerState>) {
    std::thread::spawn(crate::process::inherit(move || {
        crate::process::mark_server_thread();
        let mut last: HashMap<String, u64> = HashMap::new();
        let mut warmers: HashMap<usize, Warmer> = HashMap::new();
        loop {
            let now = now_secs();
            let read = |key: &str, known| {
                list_path(key)
                    .ok()
                    .and_then(|path| read_warm_snapshot_path(&path, known))
                    .unwrap_or(SnapshotUpdate::Missing)
            };
            for (index, config) in state.addons.script_filters.iter().enumerate() {
                if config.warm_queries.is_empty() {
                    continue;
                }
                let Some(policy) = Policy::of(config) else {
                    continue;
                };
                let report = warmers.entry(index).or_default().tick(
                    &now_secs,
                    config,
                    &policy,
                    &read,
                    &mut |view, query, still_due| {
                        warm_run(&state, config, policy.ttl, view, query, still_due)
                    },
                );
                if report.ran > 0 {
                    state.set_status(&format!("script:{index}"), report.error);
                }
            }
            if state.addons.slack.enabled {
                if let Some(interval) = state.addons.slack.warm_interval() {
                    if due(&last, "slack", now, interval.as_secs()) {
                        last.insert("slack".into(), now);
                        let _ = warm_slack(&state);
                    }
                }
            }
            if state.addons.browser_history.enabled {
                let interval = state.addons.browser_history.cache_ttl().as_secs().max(60);
                if due(&last, "browser_history", now, interval) {
                    last.insert("browser_history".into(), now);
                    let _ = warm_browser_history(&state);
                }
            }
            std::thread::sleep(WARM_TICK);
        }
    }));
}

fn due(last: &HashMap<String, u64>, source: &str, now: u64, interval: u64) -> bool {
    last.get(source)
        .is_none_or(|previous| now.saturating_sub(*previous) >= interval)
}

/// Refresh every list of a script filter now, whatever its snapshots say:
/// each warmed query and the queries their output asked to have warmed.
fn warm_script_filter(
    state: &Arc<ServerState>,
    index: usize,
    config: &ScriptFilterConfig,
) -> Result<(), String> {
    let Some(ttl) = config.cache_ttl() else {
        return Ok(());
    };
    let mut error = None;
    let mut ran = false;
    let mut warm_query = |query: &str| -> Vec<String> {
        // Asked for by a user, so it runs even while a background run has it, as F5 does
        let refresh = |view| {
            let config = script_filters::view_config(config, view);
            let forced = state.force(format!("list:{}", script_filter_list_key(&config, query)));
            run_list(&config, ttl, query, Some(forced.publication()))
        };
        // The default run names the other views, each warmed under its own key.
        let (views, asked) = match refresh(None) {
            Ok(result) => {
                ran = true;
                let default = result
                    .view
                    .or_else(|| result.views.first().map(|view| view.id.clone()));
                let views: Vec<ScriptFilterView> = result
                    .views
                    .into_iter()
                    .filter(|view| Some(&view.id) != default.as_ref())
                    .take(MAX_WARM_VIEWS)
                    .collect();
                (views, result.cache_warm)
            }
            Err(message) => {
                error.get_or_insert(format!("{}: {message}", config.name));
                (Vec::new(), Vec::new())
            }
        };
        for view in &views {
            if let Err(message) = refresh(Some(view)) {
                error.get_or_insert(format!("{} ({}): {message}", config.name, view.title));
            }
        }
        asked
    };
    let mut asked = Vec::new();
    for query in &config.warm_queries {
        asked.extend(warm_query(query));
    }
    for query in advertised_queries(config, &asked) {
        warm_query(&query);
    }
    if ran || error.is_some() {
        state.set_status(&format!("script:{index}"), error.clone());
    }
    error.map_or(Ok(()), Err)
}

/// One refresh run of a script filter list for the server. A list another
/// run holds is left to that run, and so is one `still_due` finds refreshed
/// by the time this run has it.
fn warm_run(
    state: &Arc<ServerState>,
    config: &ScriptFilterConfig,
    ttl: Duration,
    view: Option<&ScriptFilterView>,
    query: &str,
    still_due: &dyn Fn() -> bool,
) -> Run {
    let config = script_filters::view_config(config, view);
    let Some(held) = state.hold(format!("list:{}", script_filter_list_key(&config, query))) else {
        return Run::Skipped;
    };
    if !still_due() {
        return Run::Skipped;
    }
    let publication = held.publication();
    let result = run_list(&config, ttl, query, Some(publication.clone()));
    // A run a user waited on took the list over, so it owns the snapshot:
    // this one is neither the list's success nor its failure.
    if !publication.is_current() {
        return Run::Skipped;
    }
    Run::Done(result)
}

fn run_list(
    config: &ScriptFilterConfig,
    ttl: Duration,
    query: &str,
    publication: Option<Publication>,
) -> Result<ScriptFilterResult, String> {
    let mut env = config.env.clone();
    env.insert("RAFFI_REFRESH".into(), "1".into());
    server_run(script_filters::run_script_filter(
        &config.command,
        &config.args,
        &env,
        query,
        config.icon.clone(),
        list_target(config, query, ttl, publication),
        Some(SERVER_RUN_TIMEOUT),
    ))
}

fn warm_named(state: &Arc<ServerState>, source: &str) -> Result<(), String> {
    match source {
        "slack" => warm_slack(state),
        "slack:home" | "slack:unread" | "slack:threads" | "slack:activity" => {
            warm_slack_view(state, &source["slack:".len()..])
        }
        "browser_history" => warm_browser_history(state),
        other => {
            let index = other
                .strip_prefix("script:")
                .and_then(|index| index.parse::<usize>().ok())
                .filter(|index| *index < state.addons.script_filters.len())
                .or_else(|| {
                    state
                        .addons
                        .script_filters
                        .iter()
                        .position(|config| config.name == other || config.keyword == other)
                });
            if let Some(index) = index {
                if let Some(config) = state.addons.script_filters.get(index) {
                    return warm_script_filter(state, index, config);
                }
            }
            let message = format!("unknown cache source '{other}'");
            state.set_status(other, Some(message.clone()));
            Err(message)
        }
    }
}

/// Warm every enabled source at once, as the periodic loop would.
fn warm_all(state: &Arc<ServerState>) -> Result<(), String> {
    let addons = &state.addons;
    let errors: Vec<String> = std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        if addons.slack.enabled {
            jobs.push(scope.spawn(crate::process::inherit(|| warm_slack(state))));
        }
        if addons.browser_history.enabled {
            jobs.push(scope.spawn(crate::process::inherit(|| warm_browser_history(state))));
        }
        for (index, config) in addons.script_filters.iter().enumerate() {
            if !config.warm_queries.is_empty() {
                jobs.push(scope.spawn(crate::process::inherit(move || {
                    warm_script_filter(state, index, config)
                })));
            }
        }
        jobs.into_iter()
            .filter_map(|job| {
                job.join()
                    .unwrap_or_else(|_| Err("a cache refresh panicked".into()))
                    .err()
            })
            .collect()
    });
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn warm_slack(state: &Arc<ServerState>) -> Result<(), String> {
    let config = state.addons.slack.clone();
    super::slack_api::set_cache_fresh(config.cache_fresh());
    let result = super::slack::warm(&config, 10);
    state.set_status("slack", result.as_ref().err().cloned());
    result
}

/// Re-fetch one Slack view, for F5 on that view.
fn warm_slack_view(state: &Arc<ServerState>, view: &str) -> Result<(), String> {
    let config = state.addons.slack.clone();
    super::slack_api::set_cache_fresh(config.cache_fresh());
    let result = match view {
        "home" => super::slack::load_home(config, true),
        "unread" => super::slack::load_unread(config, true),
        "threads" => super::slack::load_threads(config, true),
        _ => super::slack::load_activity(config, true),
    }
    .map(|_| ());
    state.set_status("slack", result.as_ref().err().cloned());
    result
}

fn warm_browser_history(state: &Arc<ServerState>) -> Result<(), String> {
    let result = super::browser_history::warm(&state.addons.browser_history);
    state.set_status("browser_history", result.as_ref().err().cloned());
    result
}

/// Parse a raw script-filter payload for the UI.
pub(crate) fn parse_list(
    snapshot: &Snapshot,
    default_icon: Option<String>,
) -> Option<ScriptFilterResult> {
    script_filters::parse_script_filter_output(&snapshot.payload, default_icon).ok()
}

#[cfg(test)]
mod tests {
    use super::super::clipboard_policy::{Policy, PASSWORD_HINT};
    use super::super::clipboard_store::{Limits, Store, PASSWORD_MARKER};
    use super::*;

    #[test]
    fn decoded_len_does_not_count_padding() {
        assert_eq!(decoded_len("YWJj"), 3);
        assert_eq!(decoded_len("YWI="), 2);
        assert_eq!(decoded_len("YWJjZA=="), 4);
        assert_eq!(decoded_len(""), 0);
    }

    const LIMITS: Limits = Limits {
        max_items: 100,
        max_size: 1 << 30,
        max_image_size: 1 << 20,
    };
    const NOW: i64 = 1_700_000_000;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "raffi-cache-{name}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_addons_digest_changes_with_clipboard_policy() {
        let mut addons = AddonsConfig::default();
        let before = addons_digest(&addons);
        addons.clipboard.exclude_patterns = vec!["^secret:".into()];
        let after = addons_digest(&addons);
        assert_ne!(before, after);
        addons
            .clipboard
            .exclude_patterns
            .push("(?i)password".into());
        assert_ne!(after, addons_digest(&addons));

        // The recorder applies the temporary settings at startup too, so a
        // change to them makes a running server stale.
        let mut tempo = AddonsConfig::default();
        let plain = addons_digest(&tempo);
        tempo.clipboard.temporary_patterns = vec!["^OTP:".into()];
        let patterns = addons_digest(&tempo);
        assert_ne!(plain, patterns);
        tempo.clipboard.temporary_timeout = Some(30);
        let timeout = addons_digest(&tempo);
        assert_ne!(patterns, timeout);
        tempo.clipboard.temporary_password_manager = true;
        assert_ne!(timeout, addons_digest(&tempo));
    }

    #[test]
    fn snapshot_round_trip_and_expiry() {
        let dir = tempdir("snapshot");
        let path = dir.join("lists").join("key.json");
        write_snapshot_path(&path, r#"{"items":[]}"#, Duration::from_secs(300), 10, None).unwrap();
        let snapshot = read_snapshot_path(&path).unwrap();
        assert_eq!(snapshot.payload, r#"{"items":[]}"#);
        assert!(snapshot.is_fresh(Duration::from_secs(300)));
        assert!(!snapshot.is_fresh(Duration::ZERO));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn warm_snapshot_reads_payload_only_when_the_file_changes() {
        let dir = tempdir("warm-snapshot");
        let path = dir.join("snapshot.json");
        let write = |payload| {
            write_snapshot_path(&path, payload, Duration::from_secs(300), 10, None).unwrap();
        };
        write(r#"{"items":[],"view":"a"}"#);
        let Some(SnapshotUpdate::Changed(stamp, Some(_))) = read_warm_snapshot_path(&path, None)
        else {
            panic!("initial snapshot");
        };
        assert!(matches!(
            read_warm_snapshot_path(&path, Some(stamp)),
            Some(SnapshotUpdate::Unchanged)
        ));
        write(r#"{"items":[],"view":"b"}"#);
        assert!(matches!(
            read_warm_snapshot_path(&path, Some(stamp)),
            Some(SnapshotUpdate::Changed(_, Some(snapshot))) if snapshot.payload.contains(r#""view":"b""#)
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn config_digest_changes_with_config() {
        let a = ScriptFilterConfig {
            name: "a".into(),
            command: "echo".into(),
            keyword: "a".into(),
            cache_seconds: Some(1),
            ..Default::default()
        };
        let b = ScriptFilterConfig {
            cache_seconds: Some(2),
            ..a.clone()
        };
        assert_ne!(config_digest(&a), config_digest(&b));
        assert_eq!(config_digest(&a), config_digest(&a));
    }

    #[test]
    fn server_protocol_over_temporary_socket() {
        let dir = tempdir("ipc");
        let socket = dir.join("cache.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let state = Arc::new(ServerState::new(AddonsConfig::default()));
        let server_state = state.clone();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let Incoming::Request(request) = read_request(&mut stream, MAX_REQUEST_LINE) else {
                    panic!("the status request should be read");
                };
                let reply = handle_request(&server_state, request);
                let mut line = serde_json::to_vec(&reply).unwrap();
                line.push(b'\n');
                stream.write_all(&line).unwrap();
            }
        });
        let mut stream = UnixStream::connect(&socket).unwrap();
        let mut line = serde_json::to_vec(&Request::Status).unwrap();
        line.push(b'\n');
        stream.write_all(&line).unwrap();
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        server.join().unwrap();
        match serde_json::from_str::<Response>(&reply).unwrap() {
            Response::Status {
                pid,
                sources,
                protocol,
                config_digest,
                ..
            } => {
                assert_eq!(pid, std::process::id());
                assert!(sources.is_empty());
                assert_eq!(protocol, PROTOCOL);
                assert_eq!(config_digest, addons_digest(&AddonsConfig::default()));
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_digest_ignores_map_order() {
        let vars: Vec<(String, String)> = (0..16)
            .map(|index| (format!("VAR{index}"), index.to_string()))
            .collect();
        let config = |vars: Vec<(String, String)>| ScriptFilterConfig {
            name: "x".into(),
            command: "cmd".into(),
            keyword: "x".into(),
            env: vars.into_iter().collect(),
            ..Default::default()
        };
        let forward = config(vars.clone());
        let backward = config(vars.into_iter().rev().collect());
        assert_eq!(config_digest(&forward), config_digest(&backward));
    }

    #[test]
    fn list_keys_are_stable_and_include_query_and_env() {
        let config = ScriptFilterConfig {
            name: "x".into(),
            command: "cmd".into(),
            keyword: "x".into(),
            args: vec!["-j".into()],
            env: [("B".into(), "2".into()), ("A".into(), "1".into())].into(),
            ..Default::default()
        };
        let key = script_filter_list_key(&config, "q");
        assert_eq!(key, script_filter_list_key(&config, "q"));
        assert_ne!(key, script_filter_list_key(&config, "other"));
        let mut changed = config.clone();
        changed.env.insert("A".into(), "9".into());
        assert_ne!(key, script_filter_list_key(&changed, "q"));
    }

    #[test]
    fn warming_runs_every_view_of_each_query() {
        let dir = tempdir("views");
        let log = dir.join("runs");
        let config = ScriptFilterConfig {
            name: "Views".into(),
            command: "sh".into(),
            args: vec![
                "-c".into(),
                r#"echo "${RAFFI_VIEW:-default} $RAFFI_REFRESH $0" >> "$LOG"
                   echo '{"items":[],"view":"a","views":[{"id":"a"},{"id":"b"},{"id":"c"}]}'"#
                    .into(),
            ],
            keyword: "v".into(),
            env: [("LOG".into(), log.display().to_string())].into(),
            cache_seconds: Some(60),
            warm_queries: vec!["".into(), "x".into()],
            ..Default::default()
        };
        let state = Arc::new(ServerState::new(quiet_addons()));
        warm_script_filter(&state, 0, &config).unwrap();
        assert_eq!(
            fs::read_to_string(&log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            [
                "default 1 ",
                "b 1 ",
                "c 1 ",
                "default 1 x",
                "b 1 x",
                "c 1 x"
            ]
        );

        let view: ScriptFilterView = serde_json::from_str(r#"{"id":"b"}"#).unwrap();
        let key = script_filter_list_key(&config, "q");
        let viewed = script_filters::view_config(&config, Some(&view));
        assert_ne!(key, script_filter_list_key(&viewed, "q"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_forced_refresh_also_warms_the_queries_the_script_asks_for() {
        let dir = tempdir("advertised");
        let log = dir.join("runs");
        let config = ScriptFilterConfig {
            name: "Ask".into(),
            command: "sh".into(),
            args: vec![
                "-c".into(),
                r#"echo "[$1]" >> "$LOG"
                   if [ -z "$1" ]; then
                     echo '{"cache":{"warm":["review","review","other"]},"items":[]}'
                   else
                     echo '{"cache":{"warm":["deeper"]},"items":[]}'
                   fi"#
                .into(),
                "sh".into(),
            ],
            keyword: "a".into(),
            env: [("LOG".into(), log.display().to_string())].into(),
            cache_seconds: Some(60),
            warm_queries: vec!["".into()],
            ..Default::default()
        };
        let state = Arc::new(ServerState::new(quiet_addons()));
        warm_script_filter(&state, 0, &config).unwrap();
        let runs = fs::read_to_string(&log).unwrap();
        assert_eq!(
            runs.lines().collect::<Vec<_>>(),
            ["[]", "[review]", "[other]"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn quiet_addons() -> AddonsConfig {
        let mut addons = AddonsConfig::default();
        addons.slack.enabled = false;
        addons.browser_history.enabled = false;
        addons
    }

    #[test]
    fn waited_refreshes_reply_with_their_outcome() {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let unknown = Request::RefreshSource {
            source: "nope".into(),
            wait: true,
        };
        assert!(matches!(
            handle_request(&state, unknown),
            Response::Error { message, .. } if message.contains("nope")
        ));
        assert!(matches!(
            handle_request(&state, Request::RefreshAll),
            Response::Ok
        ));

        let script = |command: &str| Request::GetScriptFilter {
            key: "k".into(),
            command: command.into(),
            args: vec!["-c".into(), r#"echo '{"items":[]}'"#.into()],
            env: HashMap::new(),
            query: String::new(),
            force_refresh: true,
            ttl_secs: 60,
            wait: true,
            max_ttl_secs: None,
        };
        assert!(matches!(
            handle_request(&state, script("sh")),
            Response::ScriptFilter { stale: false, .. }
        ));
        assert!(matches!(
            handle_request(&state, script("/nonexistent/raffi-script")),
            Response::Error { .. }
        ));
    }

    #[test]
    fn refresh_all_runs_a_list_held_by_the_background_warmer() {
        let mut addons = quiet_addons();
        let config = ScriptFilterConfig {
            name: "Repos".into(),
            command: "sh".into(),
            args: vec!["-c".into(), r#"echo '{"items":[]}'"#.into()],
            keyword: "r".into(),
            cache_seconds: Some(60),
            warm_queries: vec!["".into()],
            ..Default::default()
        };
        let key = script_filter_list_key(&config, "");
        addons.script_filters.push(config);
        let state = Arc::new(ServerState::new(addons));
        let _held = state.hold(format!("list:{key}")).unwrap();
        assert!(matches!(
            handle_request(&state, Request::RefreshAll),
            Response::Ok
        ));
        assert!(state
            .source_status()
            .iter()
            .any(|status| status.source == "script:0" && status.error.is_none()));
    }

    fn snapshot_aged(age: u64, hint: Option<u64>) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            fetched_at: now_secs() - age,
            ttl_secs: 300,
            payload: String::new(),
            hint_secs: hint,
        }
    }

    #[test]
    fn snapshot_freshness_follows_the_hint_capped_by_the_current_config() {
        let secs = Duration::from_secs;
        let fresh = |age, hint, ttl, ceiling| {
            snapshot_aged(age, hint).fresh_within(secs(ttl), secs(ceiling))
        };
        // No hint: exactly the config TTL, so raising it applies at once.
        assert!(fresh(200, None, 300, 300));
        assert!(!fresh(400, None, 300, 300));
        assert!(fresh(400, None, 600, 600));
        // A hint above the TTL needs the ceiling's permission.
        assert!(!fresh(400, Some(86400), 300, 300));
        assert!(fresh(400, Some(86400), 300, 86400));
        assert!(fresh(40_000, Some(86400), 300, 86400));
        assert!(!fresh(90_000, Some(86400), 300, 86400));
        // A smaller hint asks for less caching and needs no permission.
        assert!(fresh(30, Some(60), 300, 300));
        assert!(!fresh(61, Some(60), 300, 300));
        // A ceiling below the TTL never lowers the TTL.
        assert!(fresh(200, Some(10_000), 300, 60));
        // Nothing outlives the staleness limit.
        let week = MAX_STALE.as_secs();
        assert!(!snapshot_aged(week + 10, Some(u64::MAX)).fresh_within(secs(300), secs(u64::MAX)));
    }

    #[test]
    fn snapshot_files_stay_readable_across_versions() {
        #[derive(Deserialize)]
        struct OldSnapshot {
            version: u32,
            payload: String,
        }
        let legacy: Snapshot =
            serde_json::from_str(r#"{"version":1,"fetched_at":5,"ttl_secs":60,"payload":"{}"}"#)
                .unwrap();
        assert_eq!(legacy.hint_secs, None);
        let written = serde_json::to_string(&snapshot_aged(0, Some(86400))).unwrap();
        let old: OldSnapshot = serde_json::from_str(&written).unwrap();
        assert_eq!(old.version, 1);
        assert_eq!(old.payload, "");
        assert_eq!(SNAPSHOT_VERSION, 1);
    }

    #[test]
    fn refresh_requests_carry_the_ceiling_and_old_ones_still_parse() {
        let config: ScriptFilterConfig = serde_yaml::from_str(
            "name: r\ncommand: r\nkeyword: r\ncache_seconds: 300\ncache_max_seconds: 86400\n",
        )
        .unwrap();
        let Some(Request::GetScriptFilter {
            ttl_secs,
            max_ttl_secs,
            ..
        }) = script_filter_refresh_request(&config, "q")
        else {
            panic!("a refresh request");
        };
        assert_eq!((ttl_secs, max_ttl_secs), (300, Some(86400)));
        let old: Request = serde_json::from_str(
            r#"{"type":"get_script_filter","key":"k","command":"c","args":[],"env":{},"query":"","ttl_secs":60}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            Request::GetScriptFilter {
                max_ttl_secs: None,
                ..
            }
        ));
    }

    #[test]
    fn server_list_runs_time_out() {
        let started = Instant::now();
        let result = script_filters::run_script_filter(
            "sh",
            &["-c".into(), "sleep 5".into()],
            &HashMap::new(),
            "",
            None,
            None,
            Some(Duration::from_millis(200)),
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_held_list_is_released_when_the_run_ends_or_panics() {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let held = state.hold("list:k".into()).expect("free list");
        assert!(state.hold("list:k".into()).is_none(), "one run per list");
        drop(held);
        assert!(state.hold("list:k".into()).is_some());
        let again = state.clone();
        let panicked = std::panic::catch_unwind(move || {
            let _held = again.hold("list:p".into());
            panic!("script run blew up");
        });
        assert!(panicked.is_err());
        assert!(
            state.hold("list:p".into()).is_some(),
            "released by unwinding"
        );
    }

    #[test]
    fn a_run_superseded_by_a_later_one_keeps_the_later_snapshot() {
        let dir = tempdir("publication");
        let path = dir.join("list.json");
        let state = Arc::new(ServerState::new(quiet_addons()));
        let key = "list:taken";
        let held = state.hold(key.to_string()).expect("free list");
        let background = held.publication();
        let run = |publication, title: &str| {
            let target = SnapshotTarget {
                path: path.clone(),
                ttl: Duration::from_secs(60),
                keep: 10,
                publication: Some(publication),
            };
            script_filters::run_script_filter(
                "sh",
                &[
                    "-c".into(),
                    format!(r#"printf '{{"items":[{{"title":"{title}"}}]}}'"#),
                ],
                &HashMap::new(),
                "",
                None,
                Some(target),
                None,
            )
            .expect("the script ran")
        };
        let payload = || read_snapshot_path(&path).unwrap().payload;

        run(background.clone(), "background");
        assert!(
            payload().contains("background"),
            "a run still owns its list"
        );

        // An F5 starts while that run is going, and writes its own result.
        let forced = state.force(key.to_string());
        let forced_publication = forced.publication();
        run(forced_publication.clone(), "forced");

        // The background run finishes last, on data older than the F5's.
        run(background, "stale");
        assert!(payload().contains("forced"));
        assert!(!payload().contains("stale"));

        // Two F5s: the earlier one must not overwrite the later one either.
        let later = state.force(key.to_string());
        run(later.publication(), "later");
        run(forced_publication, "earlier");
        assert!(payload().contains("later"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_prepared_snapshot_cannot_overtake_one_published_meanwhile() {
        let dir = tempdir("publication-race");
        let path = dir.join("list.json");
        let state = Arc::new(ServerState::new(quiet_addons()));
        let key = "list:race";
        let ttl = Duration::from_secs(60);
        let target = |publication| SnapshotTarget {
            path: path.clone(),
            ttl,
            keep: 10,
            publication: Some(publication),
        };
        let held = state.hold(key.to_string()).expect("free list");
        let payload = || read_snapshot_path(&path).unwrap().payload;

        // A background run finishes its script and prepares its output.
        let stale = prepare_snapshot_path(&path, r#"{"items":[{"title":"stale"}]}"#, ttl, 10, None)
            .expect("prepared");

        // An F5 starts, runs and publishes newer data while that file waits.
        let forced = state.force(key.to_string());
        target(forced.publication())
            .publish(
                prepare_snapshot_path(&path, r#"{"items":[{"title":"forced"}]}"#, ttl, 10, None)
                    .expect("prepared"),
            )
            .expect("the forced run publishes");
        assert!(payload().contains("forced"));

        // Only now does the background run rename its file into place.
        target(held.publication())
            .publish(stale)
            .expect("a superseded run is refused, not an error");
        assert!(payload().contains("forced"), "the newer snapshot stands");
        assert!(!payload().contains("stale"));

        // The refused run cleans up the file it prepared.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name != "list.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_warm_run_taken_over_by_a_forced_one_is_no_attempt() {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let mut config: ScriptFilterConfig =
            serde_yaml::from_str("name: r\ncommand: sh\nkeyword: r\ncache_seconds: 60\n").unwrap();
        config.args = vec!["-c".into(), r#"printf '{"items":[]}'"#.into()];
        let key = format!("list:{}", script_filter_list_key(&config, "q"));
        let plain = || true;
        assert!(matches!(
            warm_run(&state, &config, Duration::from_secs(60), None, "q", &plain),
            Run::Done(Ok(_))
        ));
        // The user's refresh starts once the background run holds the list.
        let forced = std::cell::RefCell::new(None);
        let still_due = || {
            forced
                .borrow_mut()
                .get_or_insert_with(|| state.force(key.clone()));
            true
        };
        let outcome = warm_run(
            &state,
            &config,
            Duration::from_secs(60),
            None,
            "q",
            &still_due,
        );
        assert!(
            matches!(outcome, Run::Skipped),
            "the forced run owns the list now"
        );
        drop(forced.into_inner());
    }

    #[test]
    fn the_warm_loop_leaves_a_list_held_by_another_run() {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let mut config: ScriptFilterConfig =
            serde_yaml::from_str("name: r\ncommand: sh\nkeyword: r\ncache_seconds: 60\n").unwrap();
        config.args = vec!["-c".into(), r#"printf '{"items":[]}'"#.into()];
        let key = script_filter_list_key(&config, "q");
        let held = state.hold(format!("list:{key}")).unwrap();
        let ttl = Duration::from_secs(60);
        let always = || true;
        let skipped = warm_run(&state, &config, ttl, None, "q", &always);
        assert!(matches!(skipped, Run::Skipped));
        drop(held);
        // Once it holds the list it checks again, and a refreshed list is left.
        let refreshed = || false;
        assert!(matches!(
            warm_run(&state, &config, ttl, None, "q", &refreshed),
            Run::Skipped
        ));
        assert!(matches!(
            warm_run(&state, &config, ttl, None, "q", &always),
            Run::Done(Ok(_))
        ));
        // A user's refresh keeps background runs away until it ends, however
        // many overlap.
        let first = state.force(format!("list:{key}"));
        let second = state.force(format!("list:{key}"));
        assert!(matches!(
            warm_run(&state, &config, ttl, None, "q", &always),
            Run::Skipped
        ));
        drop(first);
        assert!(state.hold(format!("list:{key}")).is_none());
        drop(second);
        assert!(state.hold(format!("list:{key}")).is_some());
    }

    #[test]
    fn server_runs_count_unstored_output_as_a_failure() {
        let run =
            |output: &str| server_run(script_filters::parse_script_filter_output(output, None));
        assert!(run(r#"{"items":[]}"#).is_ok());
        assert_eq!(
            run(r#"{"cache":{"seconds":0},"items":[{"title":"\u001b[31mGitHub error\u001b[0m"}]}"#)
                .unwrap_err(),
            "GitHub error"
        );
        assert_eq!(
            run(r#"{"cache":{"seconds":0},"items":[]}"#).unwrap_err(),
            "the script asked not to cache this output"
        );
        assert!(run("nope").unwrap_err().starts_with("Invalid JSON"));
    }

    #[test]
    fn waited_refresh_of_unstored_output_replies_with_an_error() {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let script = |output: &str| Request::GetScriptFilter {
            key: "k-not-stored".into(),
            command: "sh".into(),
            args: vec!["-c".into(), format!("printf '%s' '{output}'")],
            env: HashMap::new(),
            query: String::new(),
            force_refresh: true,
            ttl_secs: 60,
            wait: true,
            max_ttl_secs: None,
        };
        let Response::Error { message, code } = handle_request(
            &state,
            script(r#"{"cache":{"seconds":0},"items":[{"title":"GitHub error"}]}"#),
        ) else {
            panic!("an error reply");
        };
        assert_eq!(message, "GitHub error");
        assert_eq!(code, None);
        assert!(matches!(
            handle_request(&state, script(r#"{"items":[]}"#)),
            Response::ScriptFilter { .. }
        ));
    }

    #[test]
    fn requests_from_older_clients_still_parse() {
        let request: Request =
            serde_json::from_str(r#"{"type":"refresh_source","source":"slack"}"#).unwrap();
        assert!(matches!(
            request,
            Request::RefreshSource { wait: false, .. }
        ));
        let reply: Response = serde_json::from_str(
            r#"{"type":"status","pid":1,"version":"0","config_digest":"x","sources":[]}"#,
        )
        .unwrap();
        assert!(matches!(reply, Response::Status { protocol: 0, .. }));
    }

    fn set_store(name: &str) -> (Store, PathBuf) {
        let dir = tempdir(name);
        let store = Store::open(&dir.join("clipboard.db")).unwrap();
        (store, dir)
    }

    fn text_formats(value: &str) -> Vec<(String, Vec<u8>)> {
        vec![("text/plain".to_string(), value.as_bytes().to_vec())]
    }

    #[test]
    fn clipboard_set_requests_carry_their_bytes_unchanged() {
        let formats = [
            ("text/plain".to_string(), "héllo\n".as_bytes().to_vec()),
            (
                "image/png".to_string(),
                vec![0x89, b'P', b'N', b'G', 0, 0xff],
            ),
        ];
        let request = Request::ClipboardSet {
            formats: encode_formats(&formats),
            temporary: true,
            secret: true,
            expires_in: Some(30),
            source: Some("kitty".into()),
        };
        let line = serde_json::to_string(&request).unwrap();
        // Bytes travel base64-encoded, so a binary entry stays one JSON line.
        assert!(line.contains("aMOpbGxvCg=="), "{line}");
        let parsed: Request = serde_json::from_str(&line).unwrap();
        let Request::ClipboardSet {
            formats,
            temporary,
            secret,
            expires_in,
            source,
        } = parsed
        else {
            panic!("the set request should round trip");
        };
        assert!(temporary && secret);
        assert_eq!(expires_in, Some(30));
        assert_eq!(source.as_deref(), Some("kitty"));
        assert_eq!(
            decode_formats(&formats).unwrap(),
            [
                ("text/plain".to_string(), "héllo\n".as_bytes().to_vec()),
                (
                    "image/png".to_string(),
                    vec![0x89, b'P', b'N', b'G', 0, 0xff]
                ),
            ]
        );

        // Flags a client never sent default to off, and broken base64 is an
        // error rather than empty content.
        let bare: Request =
            serde_json::from_str(r#"{"type":"clipboard_set","formats":[]}"#).unwrap();
        assert!(matches!(
            bare,
            Request::ClipboardSet {
                temporary: false,
                secret: false,
                expires_in: None,
                source: None,
                ..
            }
        ));
        let broken = [SetFormat {
            mime: "text/plain".into(),
            data: "not base64!".into(),
        }];
        assert!(decode_formats(&broken)
            .unwrap_err()
            .contains("cannot read the `text/plain` content"));

        let reply = serde_json::to_string(&Response::ClipboardSet {
            id: Some("3fa9c1d2e0".into()),
            expires_at: Some(NOW + 60),
        })
        .unwrap();
        let reply: Response = serde_json::from_str(&reply).unwrap();
        assert!(matches!(
            reply,
            Response::ClipboardSet { id: Some(id), expires_at: Some(deadline) }
                if id == "3fa9c1d2e0" && deadline == NOW + 60
        ));
    }

    /// A connected socket pair whose ends give up instead of blocking, so a
    /// broken test fails rather than hanging.
    fn socket_pair() -> (UnixStream, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        for stream in [&client, &server] {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
        (client, server)
    }

    /// Sends `requests`, one connection each, and returns the replies.
    fn serve_over_socket(requests: &[Request]) -> Vec<Response> {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let mut replies = Vec::new();
        for request in requests {
            let (mut client, server) = socket_pair();
            let mut line = serde_json::to_vec(request).unwrap();
            line.push(b'\n');
            client.write_all(&line).unwrap();
            let mut server = server;
            let reply = match read_request(&mut server, MAX_REQUEST_LINE) {
                Incoming::Request(request) => handle_request(&state, request),
                Incoming::Refused(failure) => failure.into(),
                Incoming::Closed => panic!("the request should be read"),
            };
            let mut line = serde_json::to_vec(&reply).unwrap();
            line.push(b'\n');
            server.write_all(&line).unwrap();
            let mut reader = BufReader::new(client);
            let mut reply = String::new();
            reader.read_line(&mut reply).unwrap();
            replies.push(serde_json::from_str(&reply).unwrap());
        }
        replies
    }

    #[test]
    fn the_request_framing_bounds_the_line_the_service_reads() {
        // Room for a full text format or the largest image, base64-encoded.
        assert!(MAX_REQUEST_LINE > base64_len(super::super::clipboard_store::MAX_TEXT_BYTES));
        assert!(
            MAX_REQUEST_LINE > base64_len(50 * 1024 * 1024),
            "the default image limit"
        );
        assert_eq!(base64_len(3), 4);
        assert_eq!(base64_len(4), 8);

        let (mut client, mut server) = socket_pair();
        let line = serde_json::to_vec(&Request::Status).unwrap();
        client.write_all(&line).unwrap();
        client.write_all(b"\n").unwrap();
        match read_request(&mut server, 16) {
            Incoming::Refused(failure) => {
                assert_eq!(failure.code, ErrorCode::TooLarge);
                assert!(failure.message.contains("longer than"), "{failure:?}");
            }
            _ => panic!("a request longer than the framing allows is refused"),
        }

        // A line that fits but does not parse is refused too.
        let (mut client, mut server) = socket_pair();
        client.write_all(b"{}\n").unwrap();
        assert!(matches!(
            read_request(&mut server, 64),
            Incoming::Refused(Failure {
                code: ErrorCode::InvalidArgument,
                ..
            })
        ));

        // A peer that closes without sending anything is not worth an answer.
        let (client, mut server) = socket_pair();
        drop(client);
        assert!(matches!(read_request(&mut server, 64), Incoming::Closed));
    }

    #[test]
    fn a_set_request_is_bounded_before_its_content_is_decoded() {
        let limits = Limits {
            max_image_size: 4,
            ..LIMITS
        };
        let format = |mime: &str, encoded: &str| SetFormat {
            mime: mime.to_string(),
            data: encoded.to_string(),
        };
        // Empty content, too many formats, a MIME name that is not one, and
        // content over the limit of its type are all refused before decoding.
        assert_eq!(
            check_set_request(&[], &limits).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let many: Vec<SetFormat> = (0..MAX_SET_FORMATS + 1)
            .map(|_| format("text/plain", "aGk="))
            .collect();
        assert_eq!(
            check_set_request(&many, &limits).unwrap_err().code,
            ErrorCode::TooLarge
        );
        for mime in [
            "",
            "  ",
            "text/plain\u{0}",
            "text/plain\u{1b}",
            "no slash",
            "text/plain;",
            "/plain",
        ] {
            let error = check_set_request(&[format(mime, "aGk=")], &limits).unwrap_err();
            assert_eq!(
                error.code,
                ErrorCode::InvalidArgument,
                "{mime:?}: {error:?}"
            );
        }
        let long = format!("text/{}", "a".repeat(MAX_MIME_BYTES));
        assert_eq!(
            check_set_request(&[format(&long, "aGk=")], &limits)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        // base64 of 8 bytes is 12, over the 4 byte image limit.
        assert_eq!(
            check_set_request(&[format("image/png", "iVBORw0K")], &limits)
                .unwrap_err()
                .code,
            ErrorCode::TooLarge
        );
        let text_limits = Limits {
            max_size: 8,
            ..LIMITS
        };
        let error = check_set_request(&[format("text/plain", "aGVsbG8gd29ybGQ=")], &text_limits)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TooLarge);
        assert!(error.message.contains("of the history"), "{error:?}");
        // A bare atom name is not a MIME type but a selection offers it, as
        // the password-manager hint does.
        assert!(check_set_request(&[format("text/plain", "aGk=")], &limits).is_ok());
        assert!(check_set_request(
            &[format(
                super::super::clipboard_policy::PASSWORD_HINT,
                "aGk="
            )],
            &limits
        )
        .is_ok());
    }

    #[test]
    fn a_refused_set_does_not_stop_the_service() {
        // Both requests are refused before the server records anything, and
        // the server answers the next one as if nothing happened.
        let oversized: Vec<SetFormat> = (0..MAX_SET_FORMATS + 1)
            .map(|_| SetFormat {
                mime: "text/plain".into(),
                data: "aGk=".into(),
            })
            .collect();
        let replies = serve_over_socket(&[
            Request::ClipboardSet {
                formats: oversized,
                temporary: false,
                secret: false,
                expires_in: None,
                source: None,
            },
            Request::ClipboardSet {
                formats: vec![SetFormat {
                    mime: "text/plain\u{0}".into(),
                    data: "aGk=".into(),
                }],
                temporary: false,
                secret: false,
                expires_in: None,
                source: None,
            },
            Request::Status,
        ]);
        assert!(
            matches!(
                &replies[0],
                Response::Error {
                    code: Some(ErrorCode::TooLarge),
                    ..
                }
            ),
            "{:?}",
            replies[0]
        );
        assert!(
            matches!(
                &replies[1],
                Response::Error {
                    code: Some(ErrorCode::InvalidArgument),
                    ..
                }
            ),
            "{:?}",
            replies[1]
        );
        assert!(matches!(replies[2], Response::Status { .. }));
    }

    #[test]
    fn a_malformed_request_gets_an_answer_instead_of_a_closed_connection() {
        let state = Arc::new(ServerState::new(quiet_addons()));
        let (mut client, server) = socket_pair();
        client.write_all(b"{\"type\":\"clipboard_set\"}\n").unwrap();
        let server = std::thread::spawn(move || serve_connection(state, server));
        let mut reader = BufReader::new(client);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        server.join().unwrap();
        let reply: Response = serde_json::from_str(&reply).unwrap();
        // The MIME list is missing, so the request does not parse.
        assert!(
            matches!(
                reply,
                Response::Error {
                    code: Some(ErrorCode::InvalidArgument),
                    ..
                }
            ),
            "{reply:?}"
        );
    }

    #[test]
    fn a_set_is_recorded_where_a_copy_would_land() {
        let (mut store, dir) = set_store("set-record");
        let policy = Policy::default();
        let served = record_set(
            &mut store,
            text_formats("hello"),
            false,
            false,
            None,
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        let id = served.id.clone().expect("the content was recorded");
        assert_eq!(id.len(), 32, "a history id");
        assert_eq!(served.expires_at, None);
        assert_eq!(served.formats, text_formats("hello"));
        assert_eq!(store.count("1", NOW).unwrap(), (1, 1));

        // The same content again is the same entry, moved to the top.
        let again = record_set(
            &mut store,
            text_formats("hello"),
            false,
            false,
            None,
            None,
            &LIMITS,
            &policy,
            NOW + 5,
        )
        .unwrap();
        assert_eq!(again.id, Some(id));
        assert_eq!(store.count("1", NOW).unwrap(), (1, 1));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_temporary_set_gets_the_configured_timeout() {
        let (mut store, dir) = set_store("set-temporary");
        let policy = Policy::default();
        let served = record_set(
            &mut store,
            text_formats("one-time code"),
            true,
            false,
            None,
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert_eq!(served.expires_at, Some(NOW + policy.temporary_timeout));
        // An image has no text for a temporary pattern to match, and is
        // temporary all the same.
        let image = record_set(
            &mut store,
            vec![("image/png".to_string(), b"\x89PNG".to_vec())],
            true,
            false,
            None,
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert_eq!(image.expires_at, Some(NOW + policy.temporary_timeout));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn image_set_queues_ocr_and_temporary_set_cancels_it() {
        let (mut store, dir) = set_store("set-ocr");
        store.enable_ocr(true);
        let image = vec![("image/png".to_string(), b"image".to_vec())];
        let served = record_set(
            &mut store,
            image.clone(),
            false,
            false,
            None,
            None,
            &LIMITS,
            &Policy::default(),
            NOW,
        )
        .unwrap();
        let id = served.id.unwrap();
        let job = store.next_ocr_job().unwrap().unwrap();
        assert_eq!(store.ocr_info(&id).unwrap().unwrap().status, "pending");
        record_set(
            &mut store,
            image,
            true,
            false,
            None,
            None,
            &LIMITS,
            &Policy::default(),
            NOW,
        )
        .unwrap();
        assert!(store.ocr_info(&id).unwrap().is_none());
        assert!(!store.finish_ocr(job.id, "eng", Ok("stale".into())).unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_set_records_the_app_it_names_as_its_source() {
        let (mut store, dir) = set_store("set-source");
        let served = record_set(
            &mut store,
            text_formats("from an agent"),
            false,
            false,
            None,
            Some("pi".to_string()),
            &LIMITS,
            &Policy::default(),
            NOW,
        )
        .unwrap();
        let id = served.id.expect("the content was recorded");
        let page = store.list("", "", 0, &[], NOW).unwrap();
        let item = page
            .items
            .iter()
            .find(|item| item.id == id)
            .expect("the recorded entry");
        assert_eq!(item.source.as_deref(), Some("pi"));

        // Without a source the entry keeps the one it had, here none.
        let plain = record_set(
            &mut store,
            text_formats("from nowhere"),
            false,
            false,
            None,
            None,
            &LIMITS,
            &Policy::default(),
            NOW,
        )
        .unwrap();
        let plain = plain.id.expect("the content was recorded");
        let page = store.list("", "", 0, &[], NOW).unwrap();
        let item = page.items.iter().find(|item| item.id == plain).unwrap();
        assert_eq!(item.source, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_expiring_set_gets_its_own_lifetime() {
        let (mut store, dir) = set_store("set-expire");
        let policy = Policy::default();
        let served = record_set(
            &mut store,
            text_formats("one-time code"),
            false,
            false,
            Some(5),
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert!(served.id.is_some());
        assert_eq!(served.expires_at, Some(NOW + 5));

        // A secret with a lifetime is recorded without the opt-in, as a
        // temporary entry served with the hint.
        let secret = record_set(
            &mut store,
            text_formats("hunter2"),
            false,
            true,
            Some(30),
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert!(secret.id.is_some());
        assert_eq!(secret.expires_at, Some(NOW + 30));
        assert!(secret.formats.iter().any(|(mime, _)| mime == PASSWORD_HINT));

        // Exclusion patterns still keep it out of the history.
        let excluded = Policy {
            exclude: vec![regex::Regex::new("^secret:").unwrap()],
            ..Policy::default()
        };
        let skipped = record_set(
            &mut store,
            text_formats("secret: hunter2"),
            false,
            true,
            Some(30),
            None,
            &LIMITS,
            &excluded,
            NOW,
        )
        .unwrap();
        assert_eq!(skipped.id, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_expiring_set_with_an_unusable_lifetime_is_refused() {
        for expires_in in [0, -5, i64::MAX] {
            let replies = serve_over_socket(&[Request::ClipboardSet {
                formats: vec![SetFormat {
                    mime: "text/plain".into(),
                    data: "aGk=".into(),
                }],
                temporary: false,
                secret: false,
                expires_in: Some(expires_in),
                source: None,
            }]);
            assert!(
                matches!(
                    &replies[0],
                    Response::Error {
                        code: Some(ErrorCode::InvalidArgument),
                        message,
                    } if message.starts_with("expires_in: ")
                ),
                "{expires_in}: {:?}",
                replies[0]
            );
        }
    }

    #[test]
    fn a_set_source_the_history_cannot_show_is_refused() {
        let replies = serve_over_socket(&[
            Request::ClipboardSet {
                formats: vec![SetFormat {
                    mime: "text/plain".into(),
                    data: "aGk=".into(),
                }],
                temporary: false,
                secret: false,
                expires_in: None,
                source: Some("pi\u{0}".into()),
            },
            Request::ClipboardSet {
                formats: vec![SetFormat {
                    mime: "text/plain".into(),
                    data: "aGk=".into(),
                }],
                temporary: false,
                secret: false,
                expires_in: None,
                source: Some("   ".into()),
            },
            Request::Status,
        ]);
        assert!(
            matches!(
                &replies[0],
                Response::Error {
                    code: Some(ErrorCode::InvalidArgument),
                    ..
                }
            ),
            "{:?}",
            replies[0]
        );
        assert!(
            matches!(
                &replies[1],
                Response::Error {
                    code: Some(ErrorCode::InvalidArgument),
                    ..
                }
            ),
            "{:?}",
            replies[1]
        );
        assert!(matches!(replies[2], Response::Status { .. }));
    }

    #[test]
    fn a_secret_set_follows_the_password_manager_policy() {
        let (mut store, dir) = set_store("set-secret");
        let served = record_set(
            &mut store,
            text_formats("hunter2"),
            false,
            true,
            None,
            None,
            &LIMITS,
            &Policy::default(),
            NOW,
        )
        .unwrap();
        // Not opted in: off the history, on the clipboard with its hint.
        assert_eq!(served.id, None);
        assert_eq!(served.expires_at, None);
        assert!(served
            .formats
            .iter()
            .any(|(mime, data)| mime == PASSWORD_HINT && data.as_slice() == PASSWORD_MARKER));
        assert_eq!(store.count("1", NOW).unwrap(), (0, 0));

        // Opted in: a temporary entry, served with the hint.
        let policy = Policy {
            temporary_password_manager: true,
            ..Policy::default()
        };
        let served = record_set(
            &mut store,
            text_formats("hunter2"),
            false,
            true,
            None,
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert!(served.id.is_some());
        assert_eq!(served.expires_at, Some(NOW + policy.temporary_timeout));
        assert!(served.formats.iter().any(|(mime, _)| mime == PASSWORD_HINT));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_excluded_set_is_served_but_kept_out_of_the_history() {
        let (mut store, dir) = set_store("set-excluded");
        let policy = Policy {
            exclude: vec![regex::Regex::new("^secret:").unwrap()],
            ..Policy::default()
        };
        let served = record_set(
            &mut store,
            text_formats("secret: hunter2"),
            false,
            false,
            None,
            None,
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert_eq!(served.id, None);
        assert_eq!(served.formats, text_formats("secret: hunter2"));
        assert_eq!(store.count("1", NOW).unwrap(), (0, 0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_set_over_the_limits_is_refused() {
        let (mut store, dir) = set_store("set-too-large");
        let limits = Limits {
            max_image_size: 4,
            ..LIMITS
        };
        let error = record_set(
            &mut store,
            vec![("image/png".to_string(), b"\x89PNG\r\n".to_vec())],
            false,
            false,
            None,
            None,
            &limits,
            &Policy::default(),
            NOW,
        )
        .unwrap_err();
        assert!(error.message.contains("over the 4 byte limit"), "{error:?}");
        assert_eq!(error.code, ErrorCode::TooLarge);
        let _ = fs::remove_dir_all(dir);
    }
}
