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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
const WARM_TICK: Duration = Duration::from_secs(30);
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
    },
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
            config_digest: {
                let script_filters = config_digest(&addons.script_filters);
                let browser_history = config_digest(&addons.browser_history);
                let slack = config_digest(&addons.slack);
                stable_hash(&format!("{script_filters}:{browser_history}:{slack}"))
            },
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
    serde_json::to_string(value)
        .map(|json| stable_hash(&json))
        .unwrap_or_else(|_| stable_hash("unserializable"))
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
    let path = cache_socket_path().ok()?;
    let mut stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let mut line = serde_json::to_vec(request).ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).ok()?;
    serde_json::from_str(&reply).ok()
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
        },
        Request::GetScriptFilter {
            key,
            command,
            args,
            env,
            query,
            force_refresh,
            ttl_secs,
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
        Request::RefreshSource { source } => {
            let state = state.clone();
            std::thread::spawn(move || warm_named(&state, &source));
            Response::Ok
        }
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
                    warm_script_filter(&state, index, config);
                }
            }
            if state.addons.slack.enabled {
                if let Some(interval) = state.addons.slack.warm_interval() {
                    if due(&last, "slack", now, interval.as_secs()) {
                        last.insert("slack".into(), now);
                        warm_slack(&state);
                    }
                }
            }
            if state.addons.browser_history.enabled {
                let interval = state.addons.browser_history.cache_ttl().as_secs().max(60);
                if due(&last, "browser_history", now, interval) {
                    last.insert("browser_history".into(), now);
                    warm_browser_history(&state);
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

fn warm_script_filter(state: &Arc<DaemonState>, index: usize, config: &ScriptFilterConfig) {
    let Some(ttl) = config.cache_ttl() else {
        return;
    };
    let mut env = config.env.clone();
    env.insert("RAFFI_REFRESH".into(), "1".into());
    for query in &config.warm_queries {
        let _ = script_filters::run_script_filter(
            &config.command,
            &config.args,
            &env,
            query,
            config.icon.clone(),
            list_target(config, query, ttl),
        );
    }
    state.set_status(&format!("script:{index}"), None);
}

fn warm_named(state: &Arc<DaemonState>, source: &str) {
    match source {
        "slack" => warm_slack(state),
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
                    warm_script_filter(state, index, config);
                    return;
                }
            }
            state.set_status(other, Some(format!("unknown cache source '{other}'")));
        }
    }
}

fn warm_slack(state: &Arc<DaemonState>) {
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
    state.set_status("slack", error);
}

fn warm_browser_history(state: &Arc<DaemonState>) {
    let error = super::browser_history::warm(&state.addons.browser_history).err();
    state.set_status("browser_history", error);
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
            Response::Status { pid, sources, .. } => {
                assert_eq!(pid, std::process::id());
                assert!(sources.is_empty());
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
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
}
