//! Private snapshot caches and the optional background cache service.
//!
//! Snapshots are versioned JSON files under `$XDG_CACHE_HOME/raffi`, written
//! atomically with private permissions. The service speaks newline-delimited
//! JSON over a Unix socket; when it is absent the UI reads and writes the same
//! files directly.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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

const SNAPSHOT_VERSION: u32 = 1;
/// Snapshots older than this are discarded instead of shown while refreshing.
const MAX_STALE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LIST_KEEP: usize = 200;
const PREVIEW_KEEP: usize = 200;
const DAEMON_CLIENT_TIMEOUT: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// Waited refreshes run Slack and script filters, which can be slow.
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const CLIPBOARD_NOT_RUNNING: &str =
    "Raffi clipboard service is not running: start `raffi --cache-daemon`";
const WARM_TICK: Duration = Duration::from_secs(30);
/// Protocol revision reported by `Status`; daemons from before it existed
/// report 0 and ignore `wait`. Revision 2 adds `clipboard_select`, revision
/// 3 the `clipboard` status flag, revision 4 `clipboard_set`.
pub(crate) const PROTOCOL: u32 = 4;
/// Upper bound on the formats one `clipboard set` request may carry.
const MAX_SET_FORMATS: usize = 32;
/// Largest single format a request may carry, whatever the configured image
/// limit: the service has to bound the line it reads before decoding it.
const MAX_SET_FORMAT_BYTES: u64 = 64 * 1024 * 1024;
/// Longest request line the service reads: one format at
/// [`MAX_SET_FORMAT_BYTES`], base64-encoded, with the MIME names and the JSON
/// framing of [`MAX_SET_FORMATS`] formats around it.
const MAX_REQUEST_LINE: usize =
    base64_len(MAX_SET_FORMAT_BYTES as usize) + MAX_SET_FORMATS * (MAX_MIME_BYTES + 16) + 4096;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Length of `bytes` once base64-encoded.
const fn base64_len(bytes: usize) -> usize {
    (bytes + 2) / 3 * 4
}

/// A snapshot file to write after a successful script run.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotTarget {
    pub path: PathBuf,
    pub ttl: Duration,
    pub keep: usize,
}

/// One cached script output plus its metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub version: u32,
    pub fetched_at: u64,
    pub ttl_secs: u64,
    pub payload: String,
}

impl Snapshot {
    pub fn age(&self) -> Duration {
        Duration::from_secs(now_secs().saturating_sub(self.fetched_at))
    }

    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.age() < ttl
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

/// Requests accepted by the cache service.
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
    /// allows. The command exits, so the service has to own the selection.
    ClipboardSet {
        formats: Vec<SetFormat>,
        /// Record the entry as a temporary one.
        #[serde(default)]
        temporary: bool,
        /// The content is a password-manager secret.
        #[serde(default)]
        secret: bool,
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

/// Replies sent by the cache service.
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
        /// The service records the clipboard history.
        #[serde(default)]
        clipboard: bool,
    },
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
    /// The service does not record the clipboard.
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

pub(crate) struct DaemonState {
    pub config_digest: String,
    pub addons: AddonsConfig,
    status: Mutex<HashMap<String, SourceStatus>>,
    inflight: Mutex<HashSet<String>>,
}

impl DaemonState {
    pub fn new(addons: AddonsConfig) -> Self {
        Self {
            config_digest: addons_digest(&addons),
            addons,
            status: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
        }
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

/// Digest of the addon settings the cache service acts on, so the UI can
/// tell when a running service was started with another configuration.
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
    let base = std::env::var_os("XDG_RUNTIME_DIR")
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
) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: list_path(&script_filter_list_key(config, query)).ok()?,
        ttl,
        keep: LIST_KEEP,
    })
}

pub(crate) fn preview_target(request: &PreviewRequest, ttl: Duration) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: preview_path(&script_filter_preview_key(request)).ok()?,
        ttl,
        keep: PREVIEW_KEEP,
    })
}

pub(crate) fn read_list(key: &str) -> Option<Snapshot> {
    read_snapshot_path(&list_path(key).ok()?)
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

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
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
    fs::rename(&temp, path)?;
    Ok(())
}

pub(crate) fn write_snapshot_path(
    path: &Path,
    payload: &str,
    ttl: Duration,
    keep: usize,
) -> Result<()> {
    let snapshot = Snapshot {
        version: SNAPSHOT_VERSION,
        fetched_at: now_secs(),
        ttl_secs: ttl.as_secs(),
        payload: payload.to_string(),
    };
    let bytes = serde_json::to_vec(&snapshot)?;
    write_private_bytes(path, &bytes)?;
    if let Some(dir) = path.parent() {
        prune_snapshots(dir, keep);
    }
    Ok(())
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

/// Ask a running daemon to clear its state and snapshots. Best effort.
pub(crate) fn notify_clear() {
    let _ = try_request(&Request::Clear, DAEMON_CLIENT_TIMEOUT);
}

/// Best-effort request to a daemon; a missing or slow daemon is ignored.
pub(crate) fn try_request(request: &Request, timeout: Duration) -> Option<Response> {
    call(request, timeout).ok()
}

/// Why a request to the cache service got no usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallError {
    /// No service listens on the socket.
    Unreachable,
    /// The service failed the request, or did not answer in time.
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

/// What the UI needs to know about a running cache service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerInfo {
    pub protocol: u32,
    pub config_digest: String,
}

/// Ask whether a cache service is running. Blocking: run it off the UI thread.
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

/// Ask the cache service to put the `raffi` backend clipboard entry `id`
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

/// Whether a running cache service records the clipboard history.
/// Blocking: run it off the UI thread.
pub(crate) fn clipboard_recording() -> bool {
    clipboard_report().is_some_and(|(recording, _)| recording)
}

/// Whether a running cache service records the clipboard history, and the
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
    /// The service speaks an older protocol and cannot set the clipboard.
    DaemonOutdated(u32),
    /// No service listens on the socket.
    NotRunning,
    /// The service, or the socket, failed: its code when it gave one.
    Failed(Option<ErrorCode>, String),
}

impl std::fmt::Display for SetFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonOutdated(protocol) => write!(
                formatter,
                "the running Raffi service is from an older Raffi and cannot set the \
                 clipboard; restart `raffi --cache-daemon` (it speaks protocol \
                 {protocol}, this raffi {PROTOCOL})"
            ),
            Self::NotRunning => formatter.write_str(CLIPBOARD_NOT_RUNNING),
            Self::Failed(_, message) => formatter.write_str(message),
        }
    }
}

/// Ask the cache service to put `formats` on the clipboard. The service
/// records them in the history first when its policy allows, and owns the
/// selection afterwards, since this process exits. Blocking: run it off the
/// UI thread.
pub(crate) fn clipboard_set(
    formats: &[(String, Vec<u8>)],
    temporary: bool,
    secret: bool,
) -> Result<SetOutcome, SetFailure> {
    match probe() {
        Some(info) if info.protocol < PROTOCOL => {
            return Err(SetFailure::DaemonOutdated(info.protocol))
        }
        Some(_) => {}
        None => return Err(SetFailure::NotRunning),
    }
    let request = Request::ClipboardSet {
        formats: encode_formats(formats),
        temporary,
        secret,
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

/// Whether a cache service listens on the socket.
pub(crate) fn listening() -> bool {
    cache_socket_path().is_ok_and(|path| UnixStream::connect(path).is_ok())
}

/// Send waited refresh requests in order and report the first failure.
/// Blocking: run it off the UI thread.
pub(crate) fn refresh(requests: Vec<Request>) -> Result<(), CallError> {
    let mut errors = Vec::new();
    for request in &requests {
        if let Response::Error { message, .. } = call(request, WAIT_TIMEOUT)? {
            errors.push(message);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(CallError::Failed(errors.join("; ")))
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
    })
}

/// Handle one request. Public within the crate so protocol tests can drive it
/// over a temporary listener.
pub(crate) fn handle_request(state: &Arc<DaemonState>, request: Request) -> Response {
    match request {
        Request::Status => Response::Status {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            config_digest: state.config_digest.clone(),
            sources: state.source_status(),
            protocol: PROTOCOL,
            clipboard: super::clipboard_watch::recording(),
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
            let result = logged(&format!("script filter {name} {query:?}"), || {
                script_filters::run_script_filter(
                    &command,
                    &args,
                    &env,
                    &query,
                    None,
                    list_target_from_key(&key, ttl),
                )
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
        } => {
            let ttl = Duration::from_secs(ttl_secs);
            let snapshot = read_list(&key);
            let stale = force_refresh
                || snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !snapshot.is_fresh(ttl));
            let payload = snapshot.map(|snapshot| snapshot.payload);
            if (payload.is_none() || stale) && state.begin(&format!("list:{key}")) {
                let state = state.clone();
                let job_key = format!("list:{key}");
                std::thread::spawn(move || {
                    let result = script_filters::run_script_filter(
                        &command,
                        &args,
                        &env,
                        &query,
                        None,
                        list_target_from_key(&key, ttl),
                    );
                    state.set_status("script-filters", result.err());
                    state.finish(&job_key);
                });
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
                std::thread::spawn(move || {
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
                });
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
            std::thread::spawn(move || {
                let _ = logged(&source, || warm_named(&state, &source));
            });
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
        } => match serve_set(state, &formats, temporary, secret) {
            Ok((id, expires_at)) => Response::ClipboardSet { id, expires_at },
            Err(failure) => failure.into(),
        },
    }
}

/// Put the content of a `clipboard set` on the clipboard, recording it in the
/// history first when the policy allows, and report the entry it landed in.
fn serve_set(
    state: &Arc<DaemonState>,
    formats: &[SetFormat],
    temporary: bool,
    secret: bool,
) -> Result<(Option<String>, Option<i64>), Failure> {
    let config = &state.addons.clipboard;
    let limits = super::clipboard_store::Limits::from_config(config);
    // The request is checked before the service says whether it records at
    // all: a malformed request is refused whatever the state of the server.
    check_set_request(formats, &limits)?;
    if !super::clipboard_watch::recording() {
        return Err(Failure::new(
            ErrorCode::NotRecording,
            "the Raffi server is not recording the clipboard; \
             set addons.clipboard.backend to raffi or auto and restart it",
        ));
    }
    let policy = super::clipboard_policy::Policy::from_config(config)
        .map_err(|message| Failure::invalid(message))?;
    let formats = decode_formats(formats).map_err(Failure::invalid)?;
    let mut store = super::clipboard_store::Store::open_default()
        .map_err(|message| Failure::new(ErrorCode::Io, message))?;
    let now = super::clipboard_store::now_secs();
    let served = record_set(
        &mut store, formats, temporary, secret, &limits, &policy, now,
    )?;
    super::clipboard_watch::set(served.id.clone(), served.formats)
        .map_err(|message| Failure::new(ErrorCode::Io, message))?;
    Ok((served.id, served.expires_at))
}

/// Check the framing of a `clipboard set` request, and the size of its
/// encoded content, before any of it is decoded: the service must not
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
                 the service reads",
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
                     service reads",
                    format.mime.len()
                ),
            ));
        }
        super::clipboard_store::validate_mime(&format.mime)
            .map_err(|reason| Failure::invalid(format!("the request carries a MIME type: {reason}")))?;
        let limit = format_limit(&format.mime, limits);
        // Base64 is 4 bytes of line for every 3 of content, so the encoded
        // length alone already bounds the decoded one for all but the last
        // few bytes.
        if format.data.len() as u64 > base64_len(limit as usize) as u64 {
            return Err(too_large(&format.mime, format.data.len() as u64 / 4 * 3, limit));
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
/// that a request can never carry more than the service reads.
fn format_limit(mime: &str, limits: &super::clipboard_store::Limits) -> u64 {
    if mime.starts_with("image/") {
        limits.max_image_size.min(MAX_SET_FORMAT_BYTES)
    } else {
        super::clipboard_store::MAX_TEXT_BYTES as u64
    }
}

/// Roughly how many bytes `encoded` base64 holds.
fn decoded_len(encoded: &str) -> u64 {
    encoded.len() as u64 / 4 * 3
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
/// hint when it is a secret.
#[allow(clippy::too_many_arguments)]
fn record_set(
    store: &mut super::clipboard_store::Store,
    formats: Vec<(String, Vec<u8>)>,
    temporary: bool,
    secret: bool,
    limits: &super::clipboard_store::Limits,
    policy: &super::clipboard_policy::Policy,
    now: i64,
) -> Result<Served, Failure> {
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
        source: None,
        password_marked: secret,
    };
    let inserted = if temporary {
        store.insert_temporary(capture, limits, policy, now)
    } else {
        store.insert_filtered(capture, limits, policy, now)
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
        Err(super::clipboard_store::Skipped::TooLarge(reason)) => {
            Err(Failure::new(ErrorCode::TooLarge, format!("too large: {reason}")))
        }
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

fn list_target_from_key(key: &str, ttl: Duration) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: list_path(key).ok()?,
        ttl,
        keep: LIST_KEEP,
    })
}

fn preview_target_from_key(key: &str, ttl: Duration) -> Option<SnapshotTarget> {
    Some(SnapshotTarget {
        path: preview_path(key).ok()?,
        ttl,
        keep: PREVIEW_KEEP,
    })
}

/// What one connection carried.
enum Incoming {
    Request(Request),
    /// A line the service read but cannot serve.
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
            format!("the request is longer than the {limit} bytes the service reads"),
        ));
    }
    match serde_json::from_slice(&line) {
        Ok(request) => Incoming::Request(request),
        Err(error) => Incoming::Refused(Failure::invalid(format!(
            "cannot read the request: {error}"
        ))),
    }
}

fn serve_connection(state: Arc<DaemonState>, mut stream: UnixStream) {
    let reply = match read_request(&mut stream, MAX_REQUEST_LINE) {
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
pub(crate) fn serve(listener: UnixListener, state: Arc<DaemonState>) -> Result<()> {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || serve_connection(state, stream));
            }
            Err(error) => crate::debug_log!("cache: accept failed: {error}"),
        }
    }
    Ok(())
}

/// Run the cache service in the foreground.
pub(crate) fn run(config: ParsedConfig) -> Result<()> {
    let socket = cache_socket_path()?;
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
    let state = Arc::new(DaemonState::new(config.addons));
    eprintln!("raffi: cache service listening on {}", socket.display());
    super::hot_files::keep_warm();
    let clipboard = &state.addons.clipboard;
    if clipboard.enabled != Some(false)
        && super::clipboard_history::daemon_records(
            clipboard.backend,
            super::clipboard_history::running_cmdlines,
        )
    {
        let policy =
            super::clipboard_policy::Policy::from_config(clipboard).map_err(anyhow::Error::msg)?;
        super::clipboard_store::start_sweeper();
        super::clipboard_watch::start(
            super::clipboard_store::Limits::from_config(clipboard),
            policy,
        );
    }
    warm_loop(state.clone());
    serve(listener, state)
}

/// Start periodic warming for every source that opted in.
fn warm_loop(state: Arc<DaemonState>) {
    std::thread::spawn(move || {
        let mut last: HashMap<String, u64> = HashMap::new();
        loop {
            let now = now_secs();
            for (index, config) in state.addons.script_filters.iter().enumerate() {
                if config.warm_queries.is_empty() {
                    continue;
                }
                let Some(interval) = config.warm_interval() else {
                    continue;
                };
                let source = format!("script:{index}");
                if due(&last, &source, now, interval.as_secs()) {
                    last.insert(source, now);
                    let _ = warm_script_filter(&state, index, config);
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
    });
}

fn due(last: &HashMap<String, u64>, source: &str, now: u64, interval: u64) -> bool {
    last.get(source)
        .is_none_or(|previous| now.saturating_sub(*previous) >= interval)
}

fn warm_script_filter(
    state: &Arc<DaemonState>,
    index: usize,
    config: &ScriptFilterConfig,
) -> Result<(), String> {
    let Some(ttl) = config.cache_ttl() else {
        return Ok(());
    };
    let mut error = None;
    for query in &config.warm_queries {
        // The default run names the other views, each warmed under its own key.
        let views = match warm_script_filter_view(config, None, query, ttl) {
            Ok(result) => {
                let default = result
                    .view
                    .or_else(|| result.views.first().map(|view| view.id.clone()));
                result
                    .views
                    .into_iter()
                    .filter(|view| Some(&view.id) != default.as_ref())
                    .collect()
            }
            Err(message) => {
                error.get_or_insert(format!("{}: {message}", config.name));
                Vec::new()
            }
        };
        for view in &views {
            if let Err(message) = warm_script_filter_view(config, Some(view), query, ttl) {
                error.get_or_insert(format!("{} ({}): {message}", config.name, view.title));
            }
        }
    }
    state.set_status(&format!("script:{index}"), error.clone());
    error.map_or(Ok(()), Err)
}

fn warm_script_filter_view(
    config: &ScriptFilterConfig,
    view: Option<&ScriptFilterView>,
    query: &str,
    ttl: Duration,
) -> Result<ScriptFilterResult, String> {
    let config = script_filters::view_config(config, view);
    let mut env = config.env.clone();
    env.insert("RAFFI_REFRESH".into(), "1".into());
    script_filters::run_script_filter(
        &config.command,
        &config.args,
        &env,
        query,
        config.icon.clone(),
        list_target(&config, query, ttl),
    )
}

fn warm_named(state: &Arc<DaemonState>, source: &str) -> Result<(), String> {
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
fn warm_all(state: &Arc<DaemonState>) -> Result<(), String> {
    let addons = &state.addons;
    let errors: Vec<String> = std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        if addons.slack.enabled {
            jobs.push(scope.spawn(|| warm_slack(state)));
        }
        if addons.browser_history.enabled {
            jobs.push(scope.spawn(|| warm_browser_history(state)));
        }
        for (index, config) in addons.script_filters.iter().enumerate() {
            if !config.warm_queries.is_empty() {
                jobs.push(scope.spawn(move || warm_script_filter(state, index, config)));
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

fn warm_slack(state: &Arc<DaemonState>) -> Result<(), String> {
    let config = state.addons.slack.clone();
    super::slack_api::set_cache_fresh(config.cache_fresh());
    let mut error = None;
    for result in [
        super::slack::load_home(config.clone(), true),
        super::slack::load_unread(config.clone(), true),
        super::slack::load_threads(config.clone(), true),
        super::slack::load_activity(config.clone(), true),
    ] {
        if let Err(message) = result {
            error = Some(message);
        }
    }
    if let Err(message) = super::slack::warm_cached_previews(&config, 10) {
        error.get_or_insert(message);
    }
    state.set_status("slack", error.clone());
    error.map_or(Ok(()), Err)
}

/// Re-fetch one Slack view, for F5 on that view.
fn warm_slack_view(state: &Arc<DaemonState>, view: &str) -> Result<(), String> {
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

fn warm_browser_history(state: &Arc<DaemonState>) -> Result<(), String> {
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
        // change to them makes a running service stale.
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
        write_snapshot_path(&path, r#"{"items":[]}"#, Duration::from_secs(300), 10).unwrap();
        let snapshot = read_snapshot_path(&path).unwrap();
        assert_eq!(snapshot.payload, r#"{"items":[]}"#);
        assert!(snapshot.is_fresh(Duration::from_secs(300)));
        assert!(!snapshot.is_fresh(Duration::ZERO));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
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
    fn daemon_protocol_over_temporary_socket() {
        let dir = tempdir("ipc");
        let socket = dir.join("cache.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let state = Arc::new(DaemonState::new(AddonsConfig::default()));
        let server_state = state.clone();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let Incoming::Request(request) =
                    read_request(&mut stream, MAX_REQUEST_LINE)
                else {
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
        let state = Arc::new(DaemonState::new(quiet_addons()));
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

    fn quiet_addons() -> AddonsConfig {
        let mut addons = AddonsConfig::default();
        addons.slack.enabled = false;
        addons.browser_history.enabled = false;
        addons
    }

    #[test]
    fn waited_refreshes_reply_with_their_outcome() {
        let state = Arc::new(DaemonState::new(quiet_addons()));
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
        };
        let line = serde_json::to_string(&request).unwrap();
        // Bytes travel base64-encoded, so a binary entry stays one JSON line.
        assert!(line.contains("aMOpbGxvCg=="), "{line}");
        let parsed: Request = serde_json::from_str(&line).unwrap();
        let Request::ClipboardSet {
            formats,
            temporary,
            secret,
        } = parsed
        else {
            panic!("the set request should round trip");
        };
        assert!(temporary && secret);
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

    /// Sends `requests`, one connection each, and returns the replies.
    fn serve_over_socket(requests: &[Request]) -> Vec<Response> {
        let state = Arc::new(DaemonState::new(quiet_addons()));
        let mut replies = Vec::new();
        for request in requests {
            let (mut client, server) = UnixStream::pair().unwrap();
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
        assert!(MAX_REQUEST_LINE > base64_len(50 * 1024 * 1024), "the default image limit");
        assert_eq!(base64_len(3), 4);
        assert_eq!(base64_len(4), 8);

        let (mut client, mut server) = UnixStream::pair().unwrap();
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
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"{}\n").unwrap();
        assert!(matches!(
            read_request(&mut server, 64),
            Incoming::Refused(Failure {
                code: ErrorCode::InvalidArgument,
                ..
            })
        ));

        // An empty connection is not worth an answer.
        let (_client, mut server) = UnixStream::pair().unwrap();
        assert!(matches!(
            read_request(&mut server, 64),
            Incoming::Closed
        ));
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
            "no-slash",
            "text/plain;",
            "/plain",
        ] {
            let error = check_set_request(&[format(mime, "aGk=")], &limits).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgument, "{mime:?}: {error:?}");
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
        let text_limits = Limits { max_size: 8, ..LIMITS };
        let error = check_set_request(
            &[format("text/plain", "aGVsbG8gd29ybGQ=")],
            &text_limits,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::TooLarge);
        assert!(error.message.contains("of the history"), "{error:?}");
        assert!(check_set_request(&[format("text/plain", "aGk=")], &limits).is_ok());
    }

    #[test]
    fn a_refused_set_does_not_stop_the_service() {
        // Both requests are refused before the daemon records anything, and
        // the service answers the next one as if nothing happened.
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
            },
            Request::ClipboardSet {
                formats: vec![SetFormat {
                    mime: "text/plain\u{0}".into(),
                    data: "aGk=".into(),
                }],
                temporary: false,
                secret: false,
            },
            Request::Status,
        ]);
        assert!(
            matches!(&replies[0], Response::Error { code: Some(ErrorCode::TooLarge), .. }),
            "{:?}",
            replies[0]
        );
        assert!(
            matches!(&replies[1], Response::Error { code: Some(ErrorCode::InvalidArgument), .. }),
            "{:?}",
            replies[1]
        );
        assert!(matches!(replies[2], Response::Status { .. }));
    }

    #[test]
    fn a_malformed_request_gets_an_answer_instead_of_a_closed_connection() {
        let state = Arc::new(DaemonState::new(quiet_addons()));
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"{\"type\":\"clipboard_set\"}\n").unwrap();
        let server = std::thread::spawn(move || serve_connection(state, server));
        let mut reader = BufReader::new(client);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        server.join().unwrap();
        let reply: Response = serde_json::from_str(&reply).unwrap();
        // The MIME list is missing, so the request does not parse.
        assert!(
            matches!(reply, Response::Error { code: Some(ErrorCode::InvalidArgument), .. }),
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
            &LIMITS,
            &policy,
            NOW,
        )
        .unwrap();
        assert_eq!(image.expires_at, Some(NOW + policy.temporary_timeout));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_secret_set_follows_the_password_manager_policy() {
        let (mut store, dir) = set_store("set-secret");
        let served = record_set(
            &mut store,
            text_formats("hunter2"),
            false,
            true,
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
            &limits,
            &Policy::default(),
            NOW,
        )
        .unwrap_err();
        assert!(
            error.message.contains("over the 4 byte limit"),
            "{error:?}"
        );
        assert_eq!(error.code, ErrorCode::TooLarge);
        let _ = fs::remove_dir_all(dir);
    }
}
