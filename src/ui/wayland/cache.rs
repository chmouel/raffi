//! Private snapshot caches and the optional background cache service.
//!
//! Snapshots are versioned JSON files under `$XDG_CACHE_HOME/raffi`, written
//! atomically with private permissions. The service speaks newline-delimited
//! JSON over a Unix socket; when it is absent the UI reads and writes the same
//! files directly.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{AddonsConfig, ParsedConfig, ScriptFilterConfig};

use super::script_filters::{self, PreviewRequest};
use super::types::ScriptFilterResult;

const SNAPSHOT_VERSION: u32 = 1;
/// Snapshots older than this are discarded instead of shown while refreshing.
const MAX_STALE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LIST_KEEP: usize = 200;
const PREVIEW_KEEP: usize = 200;
const DAEMON_CLIENT_TIMEOUT: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// Waited refreshes run Slack and script filters, which can be slow.
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const WARM_TICK: Duration = Duration::from_secs(30);
/// Protocol revision reported by `Status`; daemons from before it existed
/// report 0 and ignore `wait`.
pub(crate) const PROTOCOL: u32 = 1;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

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
    },
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
    stable_hash(&format!("{script_filters}:{browser_history}:{slack}"))
}

fn cache_root() -> Result<PathBuf> {
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

/// Send waited refresh requests in order and report the first failure.
/// Blocking: run it off the UI thread.
pub(crate) fn refresh(requests: Vec<Request>) -> Result<(), CallError> {
    let mut errors = Vec::new();
    for request in &requests {
        if let Response::Error { message } = call(request, WAIT_TIMEOUT)? {
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
                Err(message) => Response::Error { message },
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
                Err(message) => Response::Error { message },
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
            Err(message) => Response::Error { message },
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
    }
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

fn read_line(stream: &mut UnixStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn serve_connection(state: Arc<DaemonState>, mut stream: UnixStream) {
    let Some(request) = read_line(&mut stream) else {
        return;
    };
    let reply = handle_request(&state, request);
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
    let mut env = config.env.clone();
    env.insert("RAFFI_REFRESH".into(), "1".into());
    let mut error = None;
    for query in &config.warm_queries {
        if let Err(message) = script_filters::run_script_filter(
            &config.command,
            &config.args,
            &env,
            query,
            config.icon.clone(),
            list_target(config, query, ttl),
        ) {
            error.get_or_insert(format!("{}: {message}", config.name));
        }
    }
    state.set_status(&format!("script:{index}"), error.clone());
    error.map_or(Ok(()), Err)
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
    use super::*;

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
                let request = read_line(&mut stream).unwrap();
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
            Response::Error { message } if message.contains("nope")
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
}
