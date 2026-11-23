//! Slack Web API access for the Slack addon: credentials read from the
//! user's commands, calls with rate limit retries, and the workspace
//! directory (users and conversations) cached on disk.
//!
//! The token and cookie stay in [`Secret`]s held in memory for the life of
//! the process; they are only sent to Slack, never logged or written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::secret_io::{run_checked, Cmd, SystemRunner};
use crate::secret::Secret;

const API: &str = "https://slack.com/api/";
/// The cache API the Slack web client uses, as for looking up people.
const EDGE_API: &str = "https://edgeapi.slack.com/";
const TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BODY: u64 = 64 * 1024 * 1024;
/// Retries of one call after a rate limit or a server error.
const MAX_RETRIES: u32 = 3;
/// Longest `Retry-After` waited for while the user looks at the list.
const MAX_RETRY_WAIT: u64 = 20;
/// Longest `Retry-After` waited for while paging through the directory,
/// which only happens when the cache is missing or old.
const MAX_DIRECTORY_RETRY_WAIT: u64 = 60;
const PAGE_SIZE: &str = "200";
const MAX_PAGES: usize = 200;
const DIRECTORY_FILE: &str = "directory.json";
const DIRECTORIES_DIR: &str = "directories";
const ACCOUNTS_DIR: &str = "accounts";
pub(super) const LIVE_FILE: &str = "live.json";
pub(super) const UNREAD_FILE: &str = "unread.json";
pub(super) const MUTED_FILE: &str = "muted.json";
pub(super) const THREADS_FILE: &str = "threads.json";
pub(super) const ACTIVITY_FILE: &str = "activity.json";
pub(super) const PREVIEW_DIR: &str = "previews";
const DIRECTORY_VERSION: u32 = 3;
/// People looked up one by one when the directory cannot list them all.
const MAX_LOOKUPS: usize = 200;
const LOOKUP_THREADS: usize = 8;
const PEOPLE_COUNT: u64 = 30;
/// Age past which cached data is refreshed, unless the config says
/// otherwise.
const DEFAULT_CACHE_FRESH: Duration = Duration::from_secs(60 * 60);
/// Age past which cached data is not shown at all, unless the freshness is
/// longer.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TokenKind {
    /// `xoxp-`: a user token from the user's own Slack app.
    User,
    /// `xoxc-`: the session token of a logged-in browser, used with the `d`
    /// cookie.
    Session,
}

pub(super) fn token_kind(token: &str) -> Result<TokenKind, String> {
    if token.starts_with("xoxp-") {
        Ok(TokenKind::User)
    } else if token.starts_with("xoxc-") {
        Ok(TokenKind::Session)
    } else if token.starts_with("xoxb-") {
        Err("Bot (xoxb-) tokens cannot search or set your status: \
             use a user (xoxp-) or session (xoxc-) token"
            .to_string())
    } else {
        Err("The Slack token must start with xoxp- or xoxc-".to_string())
    }
}

pub(super) struct Credentials {
    kind: TokenKind,
    token: Secret,
    cookie: Option<Secret>,
}

impl Credentials {
    pub fn new(token: Secret, cookie: Option<Secret>) -> Result<Self, String> {
        let kind = token_kind(token.expose().trim())?;
        let cookie = cookie.filter(|cookie| !cookie.expose().trim().is_empty());
        match &cookie {
            None if kind == TokenKind::Session => {
                return Err("A session (xoxc-) token needs the d cookie: \
                            set addons.slack.cookie_command"
                    .to_string())
            }
            Some(cookie) if cookie.expose().contains([';', '\n', '\r']) => {
                return Err("The d cookie must be the bare cookie value".to_string())
            }
            _ => {}
        }
        Ok(Self {
            kind,
            token: Secret::new(token.expose().trim().to_string()),
            cookie: cookie.map(|cookie| Secret::new(cookie.expose().trim().to_string())),
        })
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.token.expose())
    }

    fn cookie_header(&self) -> Option<String> {
        let cookie = self.cookie.as_ref()?.expose();
        let value = cookie.strip_prefix("d=").unwrap_or(cookie);
        Some(format!("d={value}"))
    }

    /// One-way SHA-256 digest of the token and cookie, used to key cached
    /// directories without writing secrets or command strings to disk.
    fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"raffi-slack-credentials-v1\0");
        let token = self.token.expose().as_bytes();
        hasher.update(&(token.len() as u64).to_le_bytes());
        hasher.update(token);
        match &self.cookie {
            Some(cookie) => {
                let bytes = cookie.expose().as_bytes();
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(bytes);
            }
            None => hasher.update(&0u64.to_le_bytes()),
        }
        hasher.finalize_hex()
    }
}

/// Minimal FIPS 180-4 SHA-256 for one-way credential fingerprints.
struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    total_len: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.block_len = data.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (slot, chunk) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *slot = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (&k, &word) in Self::K.iter().zip(&w) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, delta) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(delta);
        }
    }

    fn finalize_hex(mut self) -> String {
        let bit_len = self.total_len.wrapping_mul(8);
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        if self.block_len > 56 {
            self.block[self.block_len..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block_len = 0;
        }
        self.block[self.block_len..56].fill(0);
        self.block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        self.compress(&block);
        self.block.fill(0);
        let mut hex = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{word:08x}");
        }
        hex
    }
}

/// Run a user configured command printing a secret on its first line.
pub(super) fn read_secret_command(command: &str, what: &str) -> Result<Secret, String> {
    let runner = SystemRunner {
        cancel: Default::default(),
    };
    let cmd = Cmd::new("sh", &["-c", command])
        .timeout(COMMAND_TIMEOUT)
        .max_output(64 * 1024);
    let output = run_checked(&runner, &cmd).map_err(|error| {
        format!(
            "Cannot read the Slack {what}: {}",
            error.message().replace("sh failed", "command failed")
        )
    })?;
    output
        .stdout
        .into_secret_line()
        .filter(|secret| !secret.expose().trim().is_empty())
        .ok_or_else(|| format!("The Slack {what} command printed nothing"))
}

/// What a POST to the Web API answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Reply {
    pub status: u16,
    pub retry_after: Option<u64>,
    pub body: String,
}

/// Sends Web API calls; faked in tests.
pub(super) trait Http: Send + Sync {
    fn post(&self, method: &str, form: &[(String, String)]) -> Result<Reply, String>;
    /// Post `body` as JSON to `path` of the edge API, with the token added.
    fn post_edge(&self, path: &str, body: &Value) -> Result<Reply, String>;
}

struct UreqHttp {
    agent: ureq::Agent,
    credentials: Credentials,
}

impl Http for UreqHttp {
    fn post(&self, method: &str, form: &[(String, String)]) -> Result<Reply, String> {
        let mut request = self
            .agent
            .post(format!("{API}{method}"))
            .header("Authorization", self.credentials.authorization());
        if let Some(cookie) = self.credentials.cookie_header() {
            request = request.header("Cookie", cookie);
        }
        let response = request
            .send_form(
                form.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .map_err(|error| format!("Cannot reach Slack: {error}"))?;
        read_reply(response)
    }

    fn post_edge(&self, path: &str, body: &Value) -> Result<Reply, String> {
        let mut body = body.clone();
        body["token"] = Value::String(self.credentials.token.expose().to_string());
        let mut request = self.agent.post(format!("{EDGE_API}{path}"));
        if let Some(cookie) = self.credentials.cookie_header() {
            request = request.header("Cookie", cookie);
        }
        let response = request
            .send_json(&body)
            .map_err(|error| format!("Cannot reach Slack: {error}"))?;
        read_reply(response)
    }
}

fn read_reply(mut response: ureq::http::Response<ureq::Body>) -> Result<Reply, String> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok());
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|error| format!("Cannot read the Slack reply: {error}"))?;
    Ok(Reply {
        status,
        retry_after,
        body,
    })
}

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct Client {
    id: u64,
    credential_id: String,
    http: Box<dyn Http>,
    sleep: fn(Duration),
    kind: TokenKind,
    /// Why Slack refused the credentials, once it has.
    rejection: Mutex<Option<String>>,
    /// Who the credentials belong to, once asked.
    identity: OnceLock<Workspace>,
}

impl Client {
    pub fn new(credentials: Credentials) -> Self {
        let credential_id = credentials.fingerprint();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            id: NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed),
            credential_id,
            kind: credentials.kind,
            http: Box::new(UreqHttp { agent, credentials }),
            sleep: std::thread::sleep,
            rejection: Mutex::new(None),
            identity: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub fn fake(http: Box<dyn Http>) -> Self {
        Self::fake_with(http, TokenKind::User)
    }

    #[cfg(test)]
    pub fn fake_with(http: Box<dyn Http>, kind: TokenKind) -> Self {
        let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            id,
            credential_id: format!("fake-{id:016x}"),
            http,
            sleep: |_| {},
            kind,
            rejection: Mutex::new(None),
            identity: OnceLock::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }

    /// The error Slack gave when it refused the token, as after logging
    /// out.
    pub fn rejection(&self) -> Option<String> {
        self.rejection.lock().ok().and_then(|slot| slot.clone())
    }

    /// The account and workspace the credentials belong to, asked once.
    pub fn identity(&self) -> Result<Workspace, String> {
        if let Some(workspace) = self.identity.get() {
            return Ok(workspace.clone());
        }
        let workspace = auth_test(self)?;
        Ok(self.identity.get_or_init(|| workspace).clone())
    }

    /// The account the credentials belong to if Slack said so already;
    /// what is fetched is only cached for it.
    pub fn known_identity(&self) -> Option<&Workspace> {
        self.identity.get()
    }

    pub fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        self.call_waiting(method, params, MAX_RETRY_WAIT)
    }

    pub fn call_as<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[(&str, &str)],
    ) -> Result<T, String> {
        serde_json::from_value(self.call(method, params)?)
            .map_err(|_| format!("{method}: unexpected reply from Slack"))
    }

    /// Call `method`, waiting for rate limits up to `max_wait` seconds each
    /// time.
    fn call_waiting(
        &self,
        method: &str,
        params: &[(&str, &str)],
        max_wait: u64,
    ) -> Result<Value, String> {
        let form: Vec<(String, String)> = params
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        self.send(method, max_wait, || self.http.post(method, &form))
    }

    /// Call `path` of the edge API with the JSON `body`.
    pub fn edge(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.send(path, MAX_RETRY_WAIT, || self.http.post_edge(path, body))
    }

    pub fn is_session(&self) -> bool {
        self.kind == TokenKind::Session
    }

    /// Send a request until it is not rate limited, then check Slack's
    /// answer.
    fn send(
        &self,
        method: &str,
        max_wait: u64,
        post: impl Fn() -> Result<Reply, String>,
    ) -> Result<Value, String> {
        let mut attempt = 0;
        loop {
            let reply = post()?;
            if reply.status == 429 {
                let wait = reply.retry_after.unwrap_or(1).max(1);
                if attempt >= MAX_RETRIES || wait > max_wait {
                    return Err(format!("Slack rate limit reached, try again in {wait}s"));
                }
                crate::debug_log!("slack: {method} rate limited, waiting {wait}s");
                attempt += 1;
                (self.sleep)(Duration::from_secs(wait));
                continue;
            }
            if reply.status >= 500 && attempt < MAX_RETRIES {
                attempt += 1;
                crate::debug_log!("slack: {method} answered {}, retrying", reply.status);
                (self.sleep)(Duration::from_secs(u64::from(attempt)));
                continue;
            }
            if !(200..300).contains(&reply.status) {
                return Err(format!("{method}: Slack answered HTTP {}", reply.status));
            }
            let value: Value = serde_json::from_str(&reply.body)
                .map_err(|_| format!("{method}: unexpected reply from Slack"))?;
            if value["ok"].as_bool() == Some(true) {
                return Ok(value);
            }
            let error = value["error"].as_str().unwrap_or("unknown_error");
            if let Some(message) = rejected_message(error, self.kind) {
                crate::debug_log!("slack: {method} refused the credentials: {error}");
                if let Ok(mut slot) = self.rejection.lock() {
                    *slot = Some(message.clone());
                }
                return Err(message);
            }
            return Err(api_error(method, error, &value));
        }
    }

    /// Every item under `key` of a cursor paginated method.
    fn paginate(
        &self,
        method: &str,
        params: &[(&str, &str)],
        key: &str,
    ) -> Result<Vec<Value>, String> {
        let mut items = Vec::new();
        let mut cursor = String::new();
        for page in 0..MAX_PAGES {
            let mut page_params = params.to_vec();
            if !cursor.is_empty() {
                page_params.push(("cursor", cursor.as_str()));
            }
            let mut value = self.call_waiting(method, &page_params, MAX_DIRECTORY_RETRY_WAIT)?;
            if let Value::Array(found) = value[key].take() {
                items.extend(found);
            }
            let next = value["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            crate::debug_log!("slack: {method} page {page}, {} items", items.len());
            if next.is_empty() {
                return Ok(items);
            }
            cursor = next;
        }
        Ok(items)
    }
}

/// What to tell the user when Slack no longer accepts the credentials.
fn rejected_message(error: &str, kind: TokenKind) -> Option<String> {
    if !matches!(
        error,
        "invalid_auth" | "not_authed" | "token_revoked" | "token_expired" | "account_inactive"
    ) {
        return None;
    }
    Some(match kind {
        TokenKind::Session => format!(
            "Slack ended the browser session ({error}): log in to Slack in the browser \
             again, then update the token and d cookie that addons.slack.token_command \
             and cookie_command print"
        ),
        TokenKind::User => format!(
            "Slack refused the token ({error}): update the token that \
             addons.slack.token_command prints"
        ),
    })
}

fn api_error(method: &str, error: &str, value: &Value) -> String {
    match error {
        "missing_scope" => format!(
            "The Slack token lacks the {} scope",
            value["needed"].as_str().unwrap_or("needed")
        ),
        "ratelimited" => "Slack rate limit reached".to_string(),
        _ => format!("{method} failed: {error}"),
    }
}

/// The configured credential commands for a scope, kept in memory only so
/// switching commands within the same scope re-runs them.
type CommandKey = (Option<String>, Option<String>);

/// Most credential-bearing clients kept in memory at once; least-recently
/// used ones are dropped so repeated credential rotations never grow without
/// bound.
const MAX_CLIENTS: usize = 8;

/// Most served-window scopes remembered at once.
const MAX_SCOPES: usize = 32;

struct CachedClient {
    client: Arc<Client>,
    used: u64,
}

struct ScopeEntry {
    commands: CommandKey,
    client: Arc<Client>,
    epoch: u64,
    used: u64,
}

/// The clients of this process, keyed by their one-way credential
/// fingerprint so a new window or refresh always checks `token_command`
/// and only reuses an existing `Client` when the credentials still match.
#[derive(Default)]
struct Clients {
    by_credential: HashMap<String, CachedClient>,
    /// Per-scope build lock so concurrent loads in the same window share
    /// one credential read without holding `CLIENTS` across command execution.
    building: HashMap<Option<u64>, Arc<Mutex<()>>>,
    /// The active client for each window scope (`None` for server or
    /// standalone work, `Some(id)` for a served window).
    scopes: HashMap<Option<u64>, ScopeEntry>,
    epoch_seq: u64,
    tick: u64,
}

impl Clients {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn record_scope(&mut self, scope: Option<u64>, commands: CommandKey, client: Arc<Client>) {
        let used = self.next_tick();
        let epoch = match self.scopes.get(&scope) {
            Some(existing) if existing.client.id() == client.id() => existing.epoch,
            _ => {
                self.epoch_seq += 1;
                self.epoch_seq
            }
        };
        self.scopes.insert(
            scope,
            ScopeEntry {
                commands,
                client,
                epoch,
                used,
            },
        );
        while self.scopes.len() > MAX_SCOPES {
            let oldest = self
                .scopes
                .iter()
                .filter(|(k, _)| k.is_some())
                .min_by_key(|(_, entry)| entry.used)
                .map(|(k, _)| *k);
            match oldest {
                Some(oldest) => {
                    self.scopes.remove(&oldest);
                }
                None => break,
            }
        }
    }

    fn reuse_scope(&mut self, scope: Option<u64>, commands: &CommandKey) -> Option<Arc<Client>> {
        let used = self.next_tick();
        let entry = self.scopes.get_mut(&scope)?;
        if &entry.commands != commands || entry.client.rejection().is_some() {
            return None;
        }
        entry.used = used;
        let client = entry.client.clone();
        if let Some(cached) = self.by_credential.get_mut(client.credential_id()) {
            cached.used = used;
        }
        Some(client)
    }

    fn resolve_or_insert(
        &mut self,
        scope: Option<u64>,
        commands: CommandKey,
        credentials: Credentials,
    ) -> Arc<Client> {
        let credential_id = credentials.fingerprint();
        let used = self.next_tick();
        let client = if let Some(cached) = self
            .by_credential
            .get_mut(&credential_id)
            .filter(|entry| entry.client.rejection().is_none())
        {
            cached.used = used;
            cached.client.clone()
        } else {
            let client = Arc::new(Client::new(credentials));
            self.by_credential.insert(
                credential_id.clone(),
                CachedClient {
                    client: client.clone(),
                    used,
                },
            );
            while self.by_credential.len() > MAX_CLIENTS {
                let oldest = self
                    .by_credential
                    .iter()
                    .filter(|(k, _)| k.as_str() != credential_id.as_str())
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(oldest) => {
                        self.by_credential.remove(&oldest);
                    }
                    None => break,
                }
            }
            client
        };
        self.record_scope(scope, commands, client.clone());
        client
    }

    fn current(&self, scope: Option<u64>) -> Option<Arc<Client>> {
        self.scopes.get(&scope).map(|entry| entry.client.clone())
    }
}

static CLIENTS: Mutex<Option<Clients>> = Mutex::new(None);

fn command_key(token_command: Option<&str>, cookie_command: Option<&str>) -> CommandKey {
    (
        token_command.map(str::to_string),
        cookie_command.map(str::to_string),
    )
}

fn build_credentials(
    token_command: Option<&str>,
    cookie_command: Option<&str>,
) -> Result<Credentials, String> {
    let token_command = token_command
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .ok_or("Set addons.slack.token_command to a command printing your Slack token")?;
    let token = read_secret_command(token_command, "token")?;
    let cookie = match cookie_command.map(str::trim).filter(|c| !c.is_empty()) {
        Some(command) if token_kind(token.expose().trim()) == Ok(TokenKind::Session) => {
            Some(read_secret_command(command, "cookie")?)
        }
        _ => None,
    };
    Credentials::new(token, cookie)
}

pub(super) fn client(
    token_command: Option<&str>,
    cookie_command: Option<&str>,
    refresh: bool,
) -> Result<Arc<Client>, String> {
    let scope = crate::process::env_scope();
    let commands = command_key(token_command, cookie_command);

    let scope_lock = {
        let mut slot = CLIENTS
            .lock()
            .map_err(|_| "Slack client unavailable".to_string())?;
        let clients = slot.get_or_insert_with(Clients::default);
        // Within the same window scope, reuse the active client unless a
        // refresh was requested or Slack refused its credentials.
        if !refresh {
            if let Some(client) = clients.reuse_scope(scope, &commands) {
                return Ok(client);
            }
        }
        clients.building.entry(scope).or_default().clone()
    };

    // Hold only the per-scope lock while running credential commands so
    // `CLIENTS` stays free for the UI thread and other windows.
    let _build_guard = scope_lock
        .lock()
        .map_err(|_| "Slack client unavailable".to_string())?;

    if !refresh {
        let mut slot = CLIENTS
            .lock()
            .map_err(|_| "Slack client unavailable".to_string())?;
        let clients = slot.get_or_insert_with(Clients::default);
        if let Some(client) = clients.reuse_scope(scope, &commands) {
            if Arc::strong_count(&scope_lock) == 2 {
                clients.building.remove(&scope);
            }
            return Ok(client);
        }
    }

    let built = build_credentials(token_command, cookie_command);

    let result = match CLIENTS.lock() {
        Ok(mut slot) => {
            let clients = slot.get_or_insert_with(Clients::default);
            let client =
                built.map(|credentials| clients.resolve_or_insert(scope, commands, credentials));
            if Arc::strong_count(&scope_lock) == 2 {
                clients.building.remove(&scope);
            }
            client
        }
        Err(_) => Err("Slack client unavailable".to_string()),
    };

    result
}

/// Whether `directory` may be shown for `client`. Verified clients only show
/// their own workspace; offline unverified clients only show a directory
/// already loaded from their own credential fingerprint.
fn may_show_for_client(client: &Client, directory: &Directory) -> bool {
    if client.rejection().is_some() {
        return false;
    }
    client
        .known_identity()
        .is_none_or(|account| account.same_account(&directory.workspace))
}

/// The client built for this window (or server work), for actions run from
/// the list.
pub(super) fn current_client() -> Result<Arc<Client>, String> {
    let scope = crate::process::env_scope();
    CLIENTS
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref()?.current(scope))
        .ok_or_else(|| "Slack is not connected".to_string())
}

/// The active client for this window only if its account epoch still matches
/// `expected_epoch`, checked atomically with the client lookup so a
/// concurrent refresh cannot swap accounts in between.
pub(super) fn client_for_epoch(expected_epoch: u64) -> Result<Arc<Client>, String> {
    let scope = crate::process::env_scope();
    let (client, scope_epoch) = CLIENTS
        .lock()
        .ok()
        .and_then(|slot| {
            let entry = slot.as_ref()?.scopes.get(&scope)?;
            Some((entry.client.clone(), entry.epoch))
        })
        .ok_or_else(|| "Slack is not connected".to_string())?;
    let dir_epoch = DIRECTORIES
        .lock()
        .ok()
        .and_then(|slot| Some(slot.as_ref()?.epoch(Some(client.credential_id()))))
        .unwrap_or(0);
    let current_epoch = (scope_epoch << 32) | (dir_epoch & 0xffff_ffff);
    if current_epoch != expected_epoch {
        return Err("Slack account changed; refresh and try again".to_string());
    }
    Ok(client)
}

/// Identifies the active client for this window, if one is connected.
pub(super) fn current_client_id() -> Option<u64> {
    current_client().ok().map(|client| client.id())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Workspace {
    pub team_id: String,
    pub user_id: String,
    /// Workspace URL, such as `https://acme.slack.com/`.
    pub url: String,
    pub name: String,
    /// An Enterprise Grid organization, too large to list everyone in.
    #[serde(default)]
    pub enterprise: bool,
}

#[derive(Deserialize)]
struct AuthTest {
    team_id: String,
    user_id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    team: String,
    #[serde(default)]
    is_enterprise_install: bool,
}

impl Workspace {
    /// Whether `other` is the same person in the same workspace.
    pub fn same_account(&self, other: &Workspace) -> bool {
        self.team_id == other.team_id && self.user_id == other.user_id
    }
}

fn auth_test(client: &Client) -> Result<Workspace, String> {
    let auth: AuthTest = client.call_as("auth.test", &[])?;
    Ok(Workspace {
        team_id: auth.team_id,
        user_id: auth.user_id,
        url: auth.url,
        name: auth.team,
        enterprise: auth.is_enterprise_install,
    })
}

/// The URL of the icon of `account`'s workspace, None when it has the
/// default one. Fails when Slack answers for another team.
pub(super) fn workspace_icon(
    client: &Client,
    account: &Workspace,
) -> Result<Option<String>, String> {
    let reply = client.call("team.info", &[])?;
    let team = &reply["team"];
    let ids = [&team["id"], &team["enterprise_id"]];
    if !ids
        .iter()
        .any(|id| id.as_str() == Some(account.team_id.as_str()))
    {
        return Err("team.info answered for another team".to_string());
    }
    let icon = &team["icon"];
    if icon["image_default"].as_bool() == Some(true) {
        return Ok(None);
    }
    Ok(["image_132", "image_88", "image_68"]
        .iter()
        .filter_map(|size| icon[size].as_str())
        .find(|url| url.starts_with("https://"))
        .map(str::to_string))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct User {
    pub id: String,
    /// The handle, as in `@name`.
    pub name: String,
    pub real_name: String,
    pub display_name: String,
    pub title: String,
    pub status_text: String,
    pub status_emoji: String,
    pub tz_label: String,
    /// Offset from UTC in seconds.
    pub tz_offset: i64,
    pub avatar: String,
    /// An app or Slackbot rather than a person.
    #[serde(default)]
    pub is_bot: bool,
}

impl User {
    /// The name Slack shows for the user.
    pub fn label(&self) -> &str {
        [&self.display_name, &self.real_name, &self.name]
            .into_iter()
            .find(|name| !name.is_empty())
            .map(String::as_str)
            .unwrap_or(&self.id)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawUser {
    id: String,
    name: String,
    deleted: bool,
    is_bot: bool,
    real_name: String,
    tz_label: String,
    tz_offset: i64,
    profile: RawProfile,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawProfile {
    real_name: String,
    display_name: String,
    title: String,
    status_text: String,
    status_emoji: String,
    image_72: String,
    image_192: String,
    /// The only image the edge API gives.
    image_original: String,
}

fn parse_user(value: Value) -> Option<User> {
    let raw: RawUser = serde_json::from_value(value).ok()?;
    if raw.deleted || raw.id.is_empty() {
        return None;
    }
    Some(User {
        is_bot: raw.is_bot || raw.id == "USLACKBOT",
        id: raw.id,
        name: raw.name,
        real_name: if raw.profile.real_name.is_empty() {
            raw.real_name
        } else {
            raw.profile.real_name
        },
        display_name: raw.profile.display_name,
        title: raw.profile.title,
        status_text: raw.profile.status_text,
        status_emoji: raw.profile.status_emoji,
        tz_label: raw.tz_label,
        tz_offset: raw.tz_offset,
        avatar: [
            raw.profile.image_192,
            raw.profile.image_72,
            raw.profile.image_original.replace("_original.", "_192."),
        ]
        .into_iter()
        .find(|url| !url.is_empty())
        .unwrap_or_default(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ConversationKind {
    Channel,
    Private,
    Group,
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    /// Channel name without `#`, or the members of a group conversation.
    pub name: String,
    /// The other user of a direct conversation.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub members: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConversation {
    id: String,
    name: String,
    is_im: bool,
    is_mpim: bool,
    is_private: bool,
    is_group: bool,
    is_archived: bool,
    user: Option<String>,
    topic: RawText,
    purpose: RawText,
    num_members: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawText {
    value: String,
}

fn parse_conversation(value: Value) -> Option<Conversation> {
    let raw: RawConversation = serde_json::from_value(value).ok()?;
    if raw.is_archived || raw.id.is_empty() {
        return None;
    }
    let kind = if raw.is_im {
        ConversationKind::Direct
    } else if raw.is_mpim {
        ConversationKind::Group
    } else if raw.is_private || raw.is_group {
        ConversationKind::Private
    } else {
        ConversationKind::Channel
    };
    let name = match kind {
        ConversationKind::Group => group_name(&raw.name),
        _ => raw.name,
    };
    Some(Conversation {
        id: raw.id,
        kind,
        name,
        user: raw.user,
        topic: raw.topic.value,
        purpose: raw.purpose.value,
        members: raw.num_members,
    })
}

/// `mpdm-alice--bob--carol-1` as `alice, bob, carol`.
pub(super) fn group_name(name: &str) -> String {
    let inner = name.strip_prefix("mpdm-").unwrap_or(name);
    let inner = match inner.rsplit_once('-') {
        Some((rest, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => inner,
    };
    inner.split("--").collect::<Vec<_>>().join(", ")
}

/// The users and conversations of the workspace, as cached on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Directory {
    pub version: u32,
    /// Seconds since the epoch.
    pub fetched_at: u64,
    pub workspace: Workspace,
    pub users: Vec<User>,
    pub conversations: Vec<Conversation>,
    /// User ids looked up and not found, so they are not asked again until
    /// the directory is fetched anew.
    #[serde(default)]
    pub missing: Vec<String>,
    #[serde(skip)]
    user_index: HashMap<String, usize>,
    #[serde(skip)]
    conversation_index: HashMap<String, usize>,
}

impl Directory {
    pub fn new(
        workspace: Workspace,
        users: Vec<User>,
        conversations: Vec<Conversation>,
        fetched_at: u64,
    ) -> Self {
        let mut directory = Self {
            version: DIRECTORY_VERSION,
            fetched_at,
            workspace,
            users,
            conversations,
            ..Default::default()
        };
        directory.reindex();
        directory
    }

    fn reindex(&mut self) {
        self.user_index = self
            .users
            .iter()
            .enumerate()
            .map(|(index, user)| (user.id.clone(), index))
            .collect();
        self.conversation_index = self
            .conversations
            .iter()
            .enumerate()
            .map(|(index, conversation)| (conversation.id.clone(), index))
            .collect();
    }

    pub fn user(&self, id: &str) -> Option<&User> {
        self.user_index.get(id).map(|&index| &self.users[index])
    }

    pub fn conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversation_index
            .get(id)
            .map(|&index| &self.conversations[index])
    }

    /// The direct conversation with `user`.
    pub fn direct_with(&self, user: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|conversation| {
            conversation.kind == ConversationKind::Direct
                && conversation.user.as_deref() == Some(user)
        })
    }

    pub fn age(&self, now: u64) -> Duration {
        Duration::from_secs(now.saturating_sub(self.fetched_at))
    }

    pub fn is_fresh(&self, now: u64) -> bool {
        self.age(now) < cache_fresh()
    }
}

/// Seconds cached data stays fresh, from `addons.slack.cache_minutes`.
static CACHE_FRESH_SECS: AtomicU64 = AtomicU64::new(DEFAULT_CACHE_FRESH.as_secs());

pub(super) fn set_cache_fresh(fresh: Duration) {
    CACHE_FRESH_SECS.store(fresh.as_secs(), Ordering::Relaxed);
}

pub(super) fn cache_fresh() -> Duration {
    Duration::from_secs(CACHE_FRESH_SECS.load(Ordering::Relaxed))
}

pub(super) fn cache_max_age() -> Duration {
    CACHE_MAX_AGE.max(cache_fresh())
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

pub(super) fn fetch_directory(client: &Client) -> Result<Directory, String> {
    let workspace = client.identity()?;
    if workspace.enterprise {
        return fetch_enterprise_directory(client, workspace);
    }
    let conversations = client
        .paginate(
            "users.conversations",
            &[
                ("types", "public_channel,private_channel,mpim,im"),
                ("exclude_archived", "true"),
                ("limit", PAGE_SIZE),
            ],
            "channels",
        )?
        .into_iter()
        .filter_map(parse_conversation)
        .collect();
    let users = client
        .paginate("users.list", &[("limit", PAGE_SIZE)], "members")?
        .into_iter()
        .filter_map(parse_user)
        .collect();
    Ok(Directory::new(workspace, users, conversations, now_secs()))
}

/// An Enterprise Grid organization can have tens of thousands of people and
/// refuses to list conversations to session tokens, so the directory holds
/// the conversations the web client starts with and only the people of the
/// direct conversations; others are looked up when needed.
fn fetch_enterprise_directory(client: &Client, workspace: Workspace) -> Result<Directory, String> {
    let conversations: Vec<Conversation> = if client.is_session() {
        let mut boot = client.call_waiting("client.userBoot", &[], MAX_DIRECTORY_RETRY_WAIT)?;
        let ims = match boot["ims"].take() {
            Value::Array(ims) => ims,
            _ => Vec::new(),
        };
        let ims = ims.into_iter().map(|mut im| {
            im["is_im"] = Value::Bool(true);
            im
        });
        match boot["channels"].take() {
            Value::Array(channels) => channels,
            _ => Vec::new(),
        }
        .into_iter()
        .chain(ims)
        .filter_map(parse_conversation)
        .collect()
    } else {
        client
            .paginate(
                "users.conversations",
                &[
                    ("types", "public_channel,private_channel,mpim,im"),
                    ("exclude_archived", "true"),
                    ("limit", PAGE_SIZE),
                ],
                "channels",
            )?
            .into_iter()
            .filter_map(parse_conversation)
            .collect()
    };
    let partners: Vec<String> = conversations
        .iter()
        .filter_map(|conversation| conversation.user.clone())
        .collect();
    let (users, _) = look_up_users(client, &workspace, &partners);
    Ok(Directory::new(workspace, users, conversations, now_secs()))
}

/// The people among `ids` Slack returns: in one edge API call for session
/// tokens, else with `users.info` for each, a few at a time. The flag tells
/// whether every id got an answer, so the others are known not to exist.
fn look_up_users(client: &Client, workspace: &Workspace, ids: &[String]) -> (Vec<User>, bool) {
    let mut ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    let complete = ids.len() <= MAX_LOOKUPS;
    ids.truncate(MAX_LOOKUPS);
    if client.is_session() {
        let body = serde_json::json!({ "ids": ids, "check_interaction": true });
        match client.edge(&format!("cache/{}/users/info", workspace.team_id), &body) {
            Ok(mut reply) => {
                let users = match reply["results"].take() {
                    Value::Array(results) => results.into_iter().filter_map(parse_user).collect(),
                    _ => Vec::new(),
                };
                return (users, complete);
            }
            Err(error) => crate::debug_log!("slack: edge users/info failed: {error}"),
        }
    }
    let mut users = Vec::new();
    let mut answered = true;
    for chunk in ids.chunks(LOOKUP_THREADS) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|id| {
                    scope.spawn(crate::process::inherit(move || {
                        client.call("users.info", &[("user", id)])
                    }))
                })
                .collect();
            for (handle, id) in handles.into_iter().zip(chunk) {
                match handle.join() {
                    Ok(Ok(mut reply)) => users.extend(parse_user(reply["user"].take())),
                    Ok(Err(error)) => {
                        answered &= error.ends_with("user_not_found");
                        crate::debug_log!("slack: cannot look up {id}: {error}");
                    }
                    Err(_) => answered = false,
                }
            }
        });
    }
    (users, complete && answered)
}

/// The directory with the people among `ids` it lacks looked up, for an
/// organization whose directory holds only some people. Blocking.
pub(super) fn learn_users(
    client: &Client,
    directory: &Arc<Directory>,
    ids: &[String],
) -> Arc<Directory> {
    if !directory.workspace.enterprise {
        return directory.clone();
    }
    let mut unknown: Vec<String> = ids
        .iter()
        .filter(|id| id.starts_with(['U', 'W']) && directory.user(id).is_none())
        .filter(|id| !directory.missing.contains(id))
        .cloned()
        .collect();
    unknown.sort_unstable();
    unknown.dedup();
    if unknown.is_empty() {
        return directory.clone();
    }
    let (found, complete) = look_up_users(client, &directory.workspace, &unknown);
    if found.is_empty() && !complete {
        return directory.clone();
    }
    let mut learned = Directory::clone(directory);
    if complete {
        learned.missing.extend(
            unknown
                .into_iter()
                .filter(|id| !found.iter().any(|user| &user.id == id)),
        );
    }
    learned.users.extend(found);
    learned.reindex();
    let learned = Arc::new(learned);
    if client
        .known_identity()
        .is_some_and(|account| account.same_account(&learned.workspace))
    {
        store_directory(client.credential_id(), &learned);
    }
    learned
}

/// People of the organization matching `query`, as the web client finds
/// them; only session tokens may ask.
pub(super) fn search_people(
    client: &Client,
    workspace: &Workspace,
    query: &str,
) -> Result<Vec<User>, String> {
    let mut reply = client.edge(
        &format!("cache/{}/users/search", workspace.team_id),
        &serde_json::json!({ "query": query, "count": PEOPLE_COUNT }),
    )?;
    Ok(match reply["results"].take() {
        Value::Array(results) => results.into_iter().filter_map(parse_user).collect(),
        _ => Vec::new(),
    })
}

fn directories_dir() -> Option<PathBuf> {
    cache_path(DIRECTORIES_DIR)
}

fn directory_path_for(credential_id: &str) -> Option<PathBuf> {
    Some(directories_dir()?.join(format!("{credential_id}.json")))
}

pub(super) fn read_directory_from(path: &Path, now: u64) -> Option<Directory> {
    let data = std::fs::read(path).ok()?;
    let mut directory: Directory = serde_json::from_slice(&data).ok()?;
    if directory.version != DIRECTORY_VERSION || directory.age(now) > cache_max_age() {
        return None;
    }
    directory.reindex();
    Some(directory)
}

pub(super) fn write_directory_to(path: &Path, directory: &Directory) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_vec(directory) {
        write_private(path, &data);
    }
}

fn prune_directory_files(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .map(|path| {
            let modified = path
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            (path, modified)
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    let drop_count = files.len() - keep;
    files.sort_by_key(|(_, modified)| *modified);
    for (path, _) in files.into_iter().take(drop_count) {
        let _ = std::fs::remove_file(path);
    }
}

/// Write `data` to `path` readable by the user only, replacing it at once.
fn write_private(path: &Path, data: &[u8]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    static WRITES: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    // Each write has its own file, so writers racing never share one.
    let temp = path.with_file_name(name);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(data))
        .and_then(|()| std::fs::rename(&temp, path));
    if let Err(error) = written {
        crate::debug_log!("slack: cannot write {}: {error}", path.display());
        let _ = std::fs::remove_file(&temp);
    }
}

/// A value cached on disk for the Slack user `user` of the workspace
/// `team`, so another account never sees it.
#[derive(Serialize, Deserialize)]
struct Cached<T> {
    team: String,
    user: String,
    /// Seconds since the epoch.
    fetched_at: u64,
    value: T,
}

/// The value cached in `path` for the account of `workspace` and its age,
/// unless it is too old.
pub(super) fn read_cache_from<T: DeserializeOwned>(
    path: &Path,
    workspace: &Workspace,
    now: u64,
) -> Option<(T, Duration)> {
    let data = std::fs::read(path).ok()?;
    let cached: Cached<T> = serde_json::from_slice(&data).ok()?;
    let age = Duration::from_secs(now.saturating_sub(cached.fetched_at));
    let owned = cached.team == workspace.team_id && cached.user == workspace.user_id;
    (owned && age <= cache_max_age()).then_some((cached.value, age))
}

pub(super) fn write_cache_to<T: Serialize>(
    path: &Path,
    workspace: &Workspace,
    value: &T,
    now: u64,
) {
    let cached = Cached {
        team: workspace.team_id.clone(),
        user: workspace.user_id.clone(),
        fetched_at: now,
        value,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_vec(&cached) {
        write_private(path, &data);
    }
}

/// Change the value cached as `name` for the account of `workspace`. A
/// write racing this one may be lost; the next load writes it again.
pub(super) fn update_cache<T: Serialize + DeserializeOwned>(
    name: &str,
    workspace: &Workspace,
    update: impl FnOnce(&mut T),
) {
    if let Some(path) = account_cache_path(name, workspace) {
        update_cache_at(&path, workspace, now_secs(), update);
    }
}

pub(super) fn update_cache_at<T: Serialize + DeserializeOwned>(
    path: &Path,
    workspace: &Workspace,
    now: u64,
    update: impl FnOnce(&mut T),
) {
    if let Some((mut value, _)) = read_cache_from::<T>(path, workspace, now) {
        update(&mut value);
        write_cache_to(path, workspace, &value, now);
    }
}

/// The account known from [`Client::known_identity`], or from the cached
/// directory before `auth.test` has answered, only if the active client's
/// epoch still matches `expected_epoch`.
pub(super) fn cached_account_for_epoch(expected_epoch: u64) -> Option<Workspace> {
    let client = client_for_epoch(expected_epoch).ok()?;
    client
        .known_identity()
        .cloned()
        .or_else(|| {
            let directory = directory_for_credential(client.credential_id())?;
            may_show_for_client(&client, &directory).then(|| directory.workspace.clone())
        })
        .filter(|workspace| !workspace.team_id.is_empty() && !workspace.user_id.is_empty())
}

fn account_key(workspace: &Workspace) -> Option<String> {
    if workspace.team_id.is_empty() || workspace.user_id.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(workspace.team_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(workspace.user_id.as_bytes());
    Some(hasher.finalize_hex())
}

fn account_cache_path_in(root: &Path, name: &str, workspace: &Workspace) -> Option<PathBuf> {
    Some(
        root.join(ACCOUNTS_DIR)
            .join(account_key(workspace)?)
            .join(name),
    )
}

/// The file `name` in the Slack cache directory; none in unit tests.
pub(super) fn cache_path(name: &str) -> Option<PathBuf> {
    crate::ui::get_slack_cache_dir()
        .ok()
        .map(|dir| dir.join(name))
}

/// The per-account cache file `name` for `workspace`; none in unit tests or
/// when the account identity is unknown.
pub(super) fn account_cache_path(name: &str, workspace: &Workspace) -> Option<PathBuf> {
    let root = crate::ui::get_slack_cache_dir().ok()?;
    account_cache_path_in(&root, name, workspace)
}

/// The value cached as `name` for the account of `workspace` and its age.
pub(super) fn read_cache<T: DeserializeOwned>(
    name: &str,
    workspace: &Workspace,
) -> Option<(T, Duration)> {
    read_cache_from(&account_cache_path(name, workspace)?, workspace, now_secs())
}

pub(super) fn write_cache<T: Serialize>(name: &str, workspace: &Workspace, value: &T) {
    if let Some(path) = account_cache_path(name, workspace) {
        write_cache_to(&path, workspace, value, now_secs());
    }
}

struct CachedDirectory {
    directory: Arc<Directory>,
    used: u64,
}

/// In-memory directories and account epochs, keyed by each client's one-way
/// credential fingerprint so concurrent accounts stay isolated in memory and
/// on disk.
#[derive(Default)]
struct Directories {
    by_credential: HashMap<String, CachedDirectory>,
    epochs: HashMap<String, u64>,
    tick: u64,
}

impl Directories {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn get(&mut self, credential_id: &str) -> Option<Arc<Directory>> {
        let used = self.next_tick();
        let entry = self.by_credential.get_mut(credential_id)?;
        entry.used = used;
        Some(entry.directory.clone())
    }

    fn insert(&mut self, credential_id: String, directory: Arc<Directory>) {
        let used = self.next_tick();
        self.by_credential
            .insert(credential_id.clone(), CachedDirectory { directory, used });
        while self.by_credential.len() > MAX_CLIENTS {
            let oldest = self
                .by_credential
                .iter()
                .filter(|(k, _)| *k != &credential_id)
                .min_by_key(|(_, entry)| entry.used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(oldest) => {
                    self.by_credential.remove(&oldest);
                }
                None => break,
            }
        }
    }

    fn forget(&mut self, credential_id: &str) -> u64 {
        self.by_credential.remove(credential_id);
        let epoch = self.epochs.entry(credential_id.to_string()).or_insert(0);
        *epoch += 1;
        *epoch
    }

    fn epoch(&self, credential_id: Option<&str>) -> u64 {
        credential_id
            .and_then(|id| self.epochs.get(id).copied())
            .unwrap_or(0)
    }
}

/// The directories and account epochs kept in memory in this process.
static DIRECTORIES: Mutex<Option<Directories>> = Mutex::new(None);

fn read_disk_directory(credential_id: &str) -> Option<Arc<Directory>> {
    Some(Arc::new(read_directory_from(
        &directory_path_for(credential_id)?,
        now_secs(),
    )?))
}

fn memory_directory(credential_id: &str) -> Option<Arc<Directory>> {
    DIRECTORIES
        .lock()
        .ok()
        .and_then(|mut slot| slot.as_mut()?.get(credential_id))
}

fn remember_directory(credential_id: &str, directory: &Arc<Directory>) {
    if cfg!(test) {
        return;
    }
    if let Ok(mut slot) = DIRECTORIES.lock() {
        slot.get_or_insert_with(Directories::default)
            .insert(credential_id.to_string(), directory.clone());
    }
}

/// The cached directory for `credential_id`, from memory or its per-credential
/// file on disk. Never reads untagged legacy files.
fn directory_for_credential(credential_id: &str) -> Option<Arc<Directory>> {
    if let Some(directory) = memory_directory(credential_id) {
        return Some(directory);
    }
    let directory = read_disk_directory(credential_id)?;
    remember_directory(credential_id, &directory);
    Some(directory)
}

pub(super) fn cached_directory() -> Option<Arc<Directory>> {
    let client = current_client().ok()?;
    let directory = directory_for_credential(client.credential_id())?;
    may_show_for_client(&client, &directory).then_some(directory)
}

/// Re-read the directory another process, such as the server, wrote for the
/// current client's credentials.
pub(super) fn reload_directory() {
    let Ok(client) = current_client() else {
        return;
    };
    let Some(directory) = read_disk_directory(client.credential_id()) else {
        return;
    };
    if may_show_for_client(&client, &directory) {
        remember_directory(client.credential_id(), &directory);
    }
}

pub(super) fn account_epoch() -> u64 {
    let scope = crate::process::env_scope();
    let Some((credential_id, scope_epoch)) = CLIENTS.lock().ok().and_then(|slot| {
        let entry = slot.as_ref()?.scopes.get(&scope)?;
        Some((entry.client.credential_id().to_string(), entry.epoch))
    }) else {
        return 0;
    };
    let dir_epoch = DIRECTORIES
        .lock()
        .ok()
        .and_then(|slot| Some(slot.as_ref()?.epoch(Some(&credential_id))))
        .unwrap_or(0);
    (scope_epoch << 32) | (dir_epoch & 0xffff_ffff)
}

/// The cached directory if it belongs to the account of `client`. When Slack
/// cannot be reached (offline, not rejected), only the directory keyed to
/// `client`'s exact credential fingerprint is reused.
pub(super) fn verified_directory(client: &Client) -> Result<Option<Arc<Directory>>, String> {
    let credential_id = client.credential_id();
    let account = match client.identity() {
        Ok(account) => account,
        Err(error) if client.rejection().is_some() => return Err(error),
        Err(error) => {
            crate::debug_log!("slack: cannot tell whose the token is: {error}");
            return match directory_for_credential(credential_id) {
                Some(directory) => Ok(Some(directory)),
                None => Err(error),
            };
        }
    };
    let Some(directory) = directory_for_credential(credential_id) else {
        return Ok(None);
    };
    if directory.workspace.same_account(&account) {
        return Ok(Some(directory));
    }
    crate::debug_log!("slack: the token belongs to another account, forgetting the cache");
    forget_directory(credential_id);
    Ok(None)
}

fn forget_directory(credential_id: &str) {
    if let Ok(mut slot) = DIRECTORIES.lock() {
        slot.get_or_insert_with(Directories::default)
            .forget(credential_id);
    }
    if let Some(path) = directory_path_for(credential_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// The directory of the account of `client`, from the cache unless
/// `refresh` or it is missing or too old.
pub(super) fn directory(client: &Client, refresh: bool) -> Result<Arc<Directory>, String> {
    if let Some(directory) = verified_directory(client)? {
        if !refresh {
            return Ok(directory);
        }
    }
    let started = std::time::Instant::now();
    let directory = Arc::new(fetch_directory(client)?);
    crate::debug_log!(
        "slack: fetched {} conversations and {} people in {:?}",
        directory.conversations.len(),
        directory.users.len(),
        started.elapsed()
    );
    store_directory(client.credential_id(), &directory);
    Ok(directory)
}

fn store_directory(credential_id: &str, directory: &Arc<Directory>) {
    if let Some(path) = directory_path_for(credential_id) {
        write_directory_to(&path, directory);
        if let Some(dir) = directories_dir() {
            prune_directory_files(&dir, MAX_CLIENTS);
        }
    }
    // Like the disk cache, kept out of unit tests so one test's directory
    // does not turn into another's cached rows.
    remember_directory(credential_id, directory);
}

/// Remove everything cached but the usage counts.
pub fn clear_cache() {
    if let Ok(dir) = crate::ui::get_slack_cache_dir() {
        for file in [
            DIRECTORY_FILE,
            LIVE_FILE,
            UNREAD_FILE,
            MUTED_FILE,
            THREADS_FILE,
            ACTIVITY_FILE,
        ] {
            let _ = std::fs::remove_file(dir.join(file));
        }
        for subdir in ["avatars", PREVIEW_DIR, DIRECTORIES_DIR, ACCOUNTS_DIR] {
            let _ = std::fs::remove_dir_all(dir.join(subdir));
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A method and its form fields.
    pub type Call = (String, Vec<(String, String)>);

    /// Answers calls from a queue of replies and records them.
    #[derive(Default, Clone)]
    pub(in super::super) struct FakeHttp {
        pub replies: Arc<Mutex<VecDeque<Reply>>>,
        pub calls: Arc<Mutex<Vec<Call>>>,
    }

    impl FakeHttp {
        pub fn with(replies: Vec<(u16, Option<u64>, Value)>) -> Self {
            let fake = Self::default();
            fake.replies.lock().unwrap().extend(replies.into_iter().map(
                |(status, retry_after, body)| Reply {
                    status,
                    retry_after,
                    body: body.to_string(),
                },
            ));
            fake
        }

        pub fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }
    }

    impl Http for FakeHttp {
        fn post(&self, method: &str, form: &[(String, String)]) -> Result<Reply, String> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), form.to_vec()));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| format!("no reply for {method}"))
        }

        fn post_edge(&self, path: &str, body: &Value) -> Result<Reply, String> {
            self.post(path, &[("body".to_string(), body.to_string())])
        }
    }

    fn ok(body: Value) -> (u16, Option<u64>, Value) {
        (200, None, body)
    }

    #[test]
    fn test_token_kind_accepts_user_and_session_tokens_only() {
        assert_eq!(token_kind("xoxp-1-2-3"), Ok(TokenKind::User));
        assert_eq!(token_kind("xoxc-1-2-3"), Ok(TokenKind::Session));
        assert!(token_kind("xoxb-1").unwrap_err().contains("Bot"));
        assert!(token_kind("nope").is_err());
    }

    #[test]
    fn test_session_token_needs_cookie() {
        let error = Credentials::new(Secret::new("xoxc-1".into()), None)
            .err()
            .unwrap();
        assert!(error.contains("cookie_command"));
        let credentials = Credentials::new(
            Secret::new("xoxc-1\n".into()),
            Some(Secret::new("d=xoxd-abc".into())),
        )
        .unwrap();
        assert_eq!(credentials.authorization(), "Bearer xoxc-1");
        assert_eq!(credentials.cookie_header().as_deref(), Some("d=xoxd-abc"));
        assert!(Credentials::new(
            Secret::new("xoxc-1".into()),
            Some(Secret::new("a; b".into()))
        )
        .is_err());
    }

    #[test]
    fn test_call_retries_after_rate_limit_then_fails_past_the_limit() {
        let fake = FakeHttp::with(vec![
            (429, Some(2), Value::Null),
            ok(serde_json::json!({"ok": true, "value": 1})),
        ]);
        let client = Client::fake(Box::new(fake.clone()));
        assert_eq!(client.call("x.y", &[]).unwrap()["value"], 1);
        assert_eq!(fake.methods(), ["x.y", "x.y"]);

        let fake = FakeHttp::with(vec![(429, Some(600), Value::Null)]);
        let client = Client::fake(Box::new(fake));
        assert!(client
            .call("x.y", &[])
            .unwrap_err()
            .contains("try again in 600s"));
    }

    #[test]
    fn test_workspace_icon_checks_the_team() {
        let account = Workspace {
            team_id: "T1".to_string(),
            ..Workspace::default()
        };
        let fake = FakeHttp::with(vec![
            ok(
                serde_json::json!({"ok": true, "team": {"id": "T1", "icon": {
                "image_68": "https://a/68.png", "image_132": "https://a/132.png"}}}),
            ),
            ok(
                serde_json::json!({"ok": true, "team": {"id": "T1", "icon": {
                "image_default": true, "image_132": "https://a/default.png"}}}),
            ),
            ok(
                serde_json::json!({"ok": true, "team": {"id": "T2", "icon": {
                "image_132": "https://a/other.png"}}}),
            ),
            ok(
                serde_json::json!({"ok": true, "team": {"id": "T9", "enterprise_id": "T1",
                "icon": {"image_88": "https://a/88.png"}}}),
            ),
            ok(serde_json::json!({"ok": false, "error": "missing_scope", "needed": "team:read"})),
        ]);
        let client = Client::fake(Box::new(fake.clone()));
        assert_eq!(
            workspace_icon(&client, &account),
            Ok(Some("https://a/132.png".to_string()))
        );
        assert_eq!(workspace_icon(&client, &account), Ok(None));
        assert!(workspace_icon(&client, &account).is_err(), "another team");
        assert_eq!(
            workspace_icon(&client, &account),
            Ok(Some("https://a/88.png".to_string()))
        );
        assert!(workspace_icon(&client, &account).is_err());
        assert_eq!(fake.methods(), ["team.info"; 5]);
    }

    #[test]
    fn test_client_is_rebuilt_for_other_credential_commands() {
        let base_env = || {
            HashMap::from([(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )])
        };
        let server_scope = crate::process::ClientEnv::new(base_env());
        let first = crate::process::with_env(server_scope.clone(), || {
            let first = client(Some("printf xoxp-1"), None, false).unwrap();
            let again = client(Some("printf xoxp-1"), None, false).unwrap();
            assert!(Arc::ptr_eq(&first, &again));
            let other = client(Some("printf xoxp-2"), None, false).unwrap();
            assert!(!Arc::ptr_eq(&first, &other));
            assert!(Arc::ptr_eq(&current_client().unwrap(), &other));
            first
        });
        // A served window starts unconnected, then reuses the matching
        // credential client once its own token command runs, while staying
        // scoped separately from the server and other windows.
        let window_a = crate::process::ClientEnv::new(base_env());
        crate::process::with_env(window_a.clone(), || {
            assert!(current_client().is_err());
            assert!(current_client_id().is_none());
        });
        let served = crate::process::with_env(window_a.clone(), || {
            client(Some("printf xoxp-1"), None, false).unwrap()
        });
        assert!(Arc::ptr_eq(&served, &first));
        // Changing another scope's client does not affect window A.
        crate::process::with_env(server_scope, || {
            client(Some("printf xoxp-2"), None, false).unwrap();
        });
        crate::process::with_env(window_a.clone(), || {
            assert!(Arc::ptr_eq(&current_client().unwrap(), &served));
            assert_eq!(current_client_id(), Some(served.id()));
        });
        // A new window with a different per-launch activation token starts
        // unconnected and reuses the same client once its token matches.
        let mut env_b = base_env();
        env_b.insert("XDG_ACTIVATION_TOKEN".to_string(), "tok-2".to_string());
        let window_b = crate::process::ClientEnv::new(env_b);
        let workspace = Workspace {
            team_id: "T1".into(),
            user_id: "U1".into(),
            ..Workspace::default()
        };
        let other_workspace = Workspace {
            team_id: "T2".into(),
            user_id: "U2".into(),
            ..Workspace::default()
        };
        let own_dir = Directory::new(workspace.clone(), Vec::new(), Vec::new(), 1_000);
        let foreign_dir = Directory::new(other_workspace.clone(), Vec::new(), Vec::new(), 1_000);
        crate::process::with_env(window_b.clone(), || {
            assert!(current_client().is_err());
            let b_client = client(Some("printf xoxp-1"), None, false).unwrap();
            assert!(Arc::ptr_eq(&b_client, &served));
            assert!(may_show_for_client(&b_client, &own_dir));
        });
        // Once verified, only the matching workspace may be shown.
        let _ = served.identity.set(workspace.clone());
        assert!(may_show_for_client(&served, &own_dir));
        assert!(!may_show_for_client(&served, &foreign_dir));
        // Delayed work from window A keeps window A's client even after
        // window B switches to another token.
        let b_other = crate::process::with_env(window_b.clone(), || {
            client(Some("printf xoxp-2"), None, false).unwrap()
        });
        assert!(!Arc::ptr_eq(&b_other, &served));
        crate::process::with_env(window_a, || {
            assert!(Arc::ptr_eq(&current_client().unwrap(), &served));
        });
        crate::process::with_env(window_b, || {
            assert!(Arc::ptr_eq(&current_client().unwrap(), &b_other));
        });
    }

    #[test]
    fn test_changed_token_from_same_command_builds_new_client() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        let cmd = format!("cat {}", token_file.display());
        let env = || {
            crate::process::ClientEnv::new(HashMap::from([(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )]))
        };
        std::fs::write(&token_file, "xoxp-account-one\n").unwrap();
        let win1 = env();
        let c1 =
            crate::process::with_env(win1.clone(), || client(Some(&cmd), None, false)).unwrap();
        // Same window reuses without re-reading until refresh.
        std::fs::write(&token_file, "xoxp-account-two\n").unwrap();
        let same_win =
            crate::process::with_env(win1.clone(), || client(Some(&cmd), None, false)).unwrap();
        assert!(Arc::ptr_eq(&c1, &same_win));
        // A new window re-runs token_command and switches to the new account.
        let win2 = env();
        let c2 = crate::process::with_env(win2, || client(Some(&cmd), None, false)).unwrap();
        assert!(!Arc::ptr_eq(&c1, &c2));
        assert_ne!(c1.credential_id(), c2.credential_id());
        // Refreshing in window 1 also picks up the changed token.
        let refreshed = crate::process::with_env(win1, || client(Some(&cmd), None, true)).unwrap();
        assert!(Arc::ptr_eq(&refreshed, &c2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_other_window_does_not_wait_for_credential_commands() {
        let env_slow = crate::process::ClientEnv::new(HashMap::from([
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("RAFFI_TEST_SLOW_CLIENT".to_string(), "1".to_string()),
        ]));
        let env_other = crate::process::ClientEnv::new(HashMap::from([
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("RAFFI_TEST_OTHER_CLIENT".to_string(), "1".to_string()),
        ]));
        let handle = std::thread::spawn(move || {
            crate::process::with_env(env_slow, || {
                client(Some("sleep 0.25; printf xoxp-slow"), None, false)
            })
        });
        // Give the background thread time to enter `read_secret_command`.
        std::thread::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        crate::process::with_env(env_other, || {
            assert!(current_client().is_err());
        });
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "current_client blocked for {:?}",
            started.elapsed()
        );
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn test_verified_directory_rejects_unverified_client_on_auth_failure() {
        let fake = FakeHttp::with(vec![ok(
            serde_json::json!({"ok": false, "error": "fatal_error"}),
        )]);
        let client = Client::fake(Box::new(fake));
        assert!(verified_directory(&client).is_err());
    }

    #[test]
    fn test_clients_cache_evicts_least_recently_used() {
        let mut clients = Clients::default();
        let mut ids = Vec::new();
        for i in 0..(MAX_CLIENTS + 3) {
            let credentials = Credentials::new(Secret::new(format!("xoxp-lru-{i}")), None).unwrap();
            let id = credentials.fingerprint();
            ids.push(id);
            clients.resolve_or_insert(
                Some(i as u64),
                (Some(format!("cmd-{i}")), None),
                credentials,
            );
        }
        assert_eq!(clients.by_credential.len(), MAX_CLIENTS);
        assert!(!clients.by_credential.contains_key(&ids[0]));
        assert!(clients.by_credential.contains_key(&ids[MAX_CLIENTS + 2]));
    }

    #[test]
    fn test_clients_scope_epoch_changes_on_account_switch() {
        let mut clients = Clients::default();
        let commands = (Some("token".into()), None);
        let cred = |token: &str| Credentials::new(Secret::new(token.into()), None).unwrap();

        let _ = clients.resolve_or_insert(Some(1), commands.clone(), cred("xoxp-switch-a"));
        let epoch_a = clients.scopes[&Some(1)].epoch;
        // Refreshing with the same credentials keeps the same scope epoch.
        let _ = clients.resolve_or_insert(Some(1), commands.clone(), cred("xoxp-switch-a"));
        assert_eq!(clients.scopes[&Some(1)].epoch, epoch_a);

        // Switching scope 1 to another account bumps only scope 1's epoch.
        let _ = clients.resolve_or_insert(Some(2), commands.clone(), cred("xoxp-switch-b"));
        let epoch_other = clients.scopes[&Some(2)].epoch;
        let _ = clients.resolve_or_insert(Some(1), commands, cred("xoxp-switch-b"));
        assert_ne!(clients.scopes[&Some(1)].epoch, epoch_a);
        assert_eq!(clients.scopes[&Some(2)].epoch, epoch_other);
    }

    #[test]
    fn test_client_for_epoch_atomically_matches_and_rejects_switched_account() {
        let env = crate::process::ClientEnv::new(HashMap::from([
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("RAFFI_TEST_CLIENT_FOR_EPOCH".to_string(), "1".to_string()),
        ]));
        crate::process::with_env(env, || {
            let client_a = client(Some("printf xoxp-atomic-a"), None, false).unwrap();
            let epoch_a = account_epoch();
            let resolved = client_for_epoch(epoch_a).unwrap();
            assert!(Arc::ptr_eq(&resolved, &client_a));

            let client_b = client(Some("printf xoxp-atomic-b"), None, true).unwrap();
            let epoch_b = account_epoch();
            assert_ne!(epoch_a, epoch_b);
            assert!(client_for_epoch(epoch_a).is_err());
            assert!(Arc::ptr_eq(&client_for_epoch(epoch_b).unwrap(), &client_b));

            let _ = client_b.identity.set(Workspace {
                team_id: "T2".into(),
                user_id: "U2".into(),
                ..Workspace::default()
            });
            assert!(cached_account_for_epoch(epoch_a).is_none());
            assert_eq!(
                cached_account_for_epoch(epoch_b).map(|w| w.team_id),
                Some("T2".to_string())
            );
        });
    }

    #[test]
    fn test_sha256_and_credential_fingerprint() {
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            hasher.finalize_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let a = Credentials::new(Secret::new("xoxp-account-a".into()), None).unwrap();
        let b = Credentials::new(Secret::new("xoxp-account-b".into()), None).unwrap();
        let fp_a = a.fingerprint();
        let fp_b = b.fingerprint();
        assert_eq!(fp_a.len(), 64);
        assert_ne!(fp_a, fp_b);
        assert!(!fp_a.contains("xoxp-"));
    }

    #[test]
    fn test_directories_and_epochs_stay_isolated_per_account() {
        let cred_a = Credentials::new(Secret::new("xoxp-a".into()), None)
            .unwrap()
            .fingerprint();
        let cred_b = Credentials::new(Secret::new("xoxp-b".into()), None)
            .unwrap()
            .fingerprint();
        let dir_a = Arc::new(Directory::new(
            Workspace {
                team_id: "T1".into(),
                user_id: "U1".into(),
                ..Workspace::default()
            },
            Vec::new(),
            Vec::new(),
            1_000,
        ));
        let dir_b = Arc::new(Directory::new(
            Workspace {
                team_id: "T2".into(),
                user_id: "U2".into(),
                ..Workspace::default()
            },
            Vec::new(),
            Vec::new(),
            1_000,
        ));
        let mut store = Directories::default();
        store.insert(cred_a.clone(), dir_a.clone());
        store.insert(cred_b.clone(), dir_b.clone());
        assert_eq!(store.get(&cred_a).unwrap().workspace.team_id, "T1");
        assert_eq!(store.get(&cred_b).unwrap().workspace.team_id, "T2");
        assert_eq!(store.epoch(Some(&cred_a)), 0);
        assert_eq!(store.epoch(Some(&cred_b)), 0);
        // Forgetting account A bumps only A's epoch and leaves B intact.
        assert_eq!(store.forget(&cred_a), 1);
        assert!(store.get(&cred_a).is_none());
        assert_eq!(store.get(&cred_b).unwrap().workspace.team_id, "T2");
        assert_eq!(store.epoch(Some(&cred_a)), 1);
        assert_eq!(store.epoch(Some(&cred_b)), 0);
        // Per-credential disk files keep both accounts without overwriting.
        let dir = std::env::temp_dir().join(format!("raffi-slack-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path_a = dir.join(format!("{cred_a}.json"));
        let path_b = dir.join(format!("{cred_b}.json"));
        write_directory_to(&path_a, &dir_a);
        write_directory_to(&path_b, &dir_b);
        assert_eq!(
            read_directory_from(&path_a, 1_060)
                .unwrap()
                .workspace
                .team_id,
            "T1"
        );
        assert_eq!(
            read_directory_from(&path_b, 1_060)
                .unwrap()
                .workspace
                .team_id,
            "T2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_call_maps_slack_errors() {
        let fake = FakeHttp::with(vec![
            ok(serde_json::json!({"ok": false, "error": "missing_scope", "needed": "search:read"})),
            ok(serde_json::json!({"ok": false, "error": "invalid_auth"})),
        ]);
        let client = Client::fake(Box::new(fake));
        assert_eq!(
            client.call("search.messages", &[]).unwrap_err(),
            "The Slack token lacks the search:read scope"
        );
        assert_eq!(client.rejection(), None);
        let error = client.call("auth.test", &[]).unwrap_err();
        assert!(error.contains("token_command"));
        assert_eq!(client.rejection(), Some(error));
    }

    #[test]
    fn test_logged_out_session_asks_for_a_new_token_and_cookie() {
        let fake = FakeHttp::with(vec![ok(
            serde_json::json!({"ok": false, "error": "invalid_auth"}),
        )]);
        let client = Client::fake_with(Box::new(fake), TokenKind::Session);
        let error = client.call("users.profile.get", &[]).unwrap_err();
        assert!(error.contains("log in to Slack"), "{error}");
        assert!(error.contains("cookie_command"), "{error}");
    }

    #[test]
    fn test_enterprise_directory_boots_and_looks_up_direct_partners() {
        let fake = FakeHttp::with(vec![
            ok(serde_json::json!({
                "ok": true, "team_id": "E1", "user_id": "U1",
                "url": "https://acme.enterprise.slack.com/", "team": "Acme",
                "is_enterprise_install": true
            })),
            ok(serde_json::json!({
                "ok": true,
                "channels": [
                    {"id": "C1", "name": "general", "is_channel": true},
                    {"id": "C2", "name": "mpdm-alice--bob-1", "is_channel": true, "is_mpim": true},
                ],
                "ims": [{"id": "D1", "user": "U2", "is_open": true}]
            })),
            ok(serde_json::json!({
                "ok": true,
                "results": [{"id": "U2", "name": "alice", "profile": {
                    "real_name": "Alice A",
                    "image_original": "https://avatars.slack-edge.com/a_original.png"
                }}]
            })),
        ]);
        let client = Client::fake_with(Box::new(fake.clone()), TokenKind::Session);
        let directory = fetch_directory(&client).unwrap();
        assert!(directory.workspace.enterprise);
        assert_eq!(
            fake.methods(),
            ["auth.test", "client.userBoot", "cache/E1/users/info"]
        );
        assert_eq!(
            directory.user("U2").unwrap().avatar,
            "https://avatars.slack-edge.com/a_192.png"
        );
        assert_eq!(
            directory.conversation("C2").unwrap().kind,
            ConversationKind::Group
        );
        assert_eq!(directory.direct_with("U2").unwrap().id, "D1");
        assert_eq!(directory.user("U2").unwrap().label(), "Alice A");
    }

    #[test]
    fn test_search_people_asks_the_edge_api() {
        let fake = FakeHttp::with(vec![ok(serde_json::json!({
            "ok": true,
            "results": [{"id": "U9", "name": "zed", "profile": {"real_name": "Zed Z"}}]
        }))]);
        let client = Client::fake_with(Box::new(fake.clone()), TokenKind::Session);
        let workspace = Workspace {
            team_id: "E1".to_string(),
            enterprise: true,
            ..Workspace::default()
        };
        let people = search_people(&client, &workspace, "zed").unwrap();
        assert_eq!(people[0].label(), "Zed Z");
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].0, "cache/E1/users/search");
        assert!(calls[0].1[0].1.contains("\"query\":\"zed\""));
    }

    #[test]
    fn test_fetch_directory_pages_and_filters() {
        let fake = FakeHttp::with(vec![
            ok(serde_json::json!({
                "ok": true, "team_id": "T1", "user_id": "U1",
                "url": "https://acme.slack.com/", "team": "Acme"
            })),
            ok(serde_json::json!({
                "ok": true,
                "channels": [
                    {"id": "C1", "name": "general", "topic": {"value": "Hi"}, "num_members": 4},
                    {"id": "G1", "name": "mpdm-alice--bob-1", "is_mpim": true},
                ],
                "response_metadata": {"next_cursor": "next"}
            })),
            ok(serde_json::json!({
                "ok": true,
                "channels": [
                    {"id": "D1", "is_im": true, "user": "U2"},
                    {"id": "C2", "name": "old", "is_archived": true},
                    {"id": "C3", "name": "secret", "is_private": true},
                ],
                "response_metadata": {"next_cursor": ""}
            })),
            ok(serde_json::json!({
                "ok": true,
                "members": [
                    {"id": "U2", "name": "alice", "tz_offset": 3600,
                     "profile": {"real_name": "Alice A", "display_name": "", "image_192": "https://a/1.png"}},
                    {"id": "U3", "name": "gone", "deleted": true},
                    {"id": "B1", "name": "bot", "is_bot": true},
                    {"id": "USLACKBOT", "name": "slackbot"},
                ]
            })),
        ]);
        let client = Client::fake(Box::new(fake.clone()));
        let directory = fetch_directory(&client).unwrap();
        assert_eq!(directory.workspace.team_id, "T1");
        let ids: Vec<_> = directory
            .conversations
            .iter()
            .map(|c| (c.id.as_str(), c.kind))
            .collect();
        assert_eq!(
            ids,
            [
                ("C1", ConversationKind::Channel),
                ("G1", ConversationKind::Group),
                ("D1", ConversationKind::Direct),
                ("C3", ConversationKind::Private),
            ]
        );
        assert_eq!(directory.conversation("C1").unwrap().members, Some(4));
        assert_eq!(directory.conversation("G1").unwrap().name, "alice, bob");
        let users: Vec<_> = directory
            .users
            .iter()
            .map(|user| (user.id.as_str(), user.is_bot))
            .collect();
        assert_eq!(users, [("U2", false), ("B1", true), ("USLACKBOT", true)]);
        assert_eq!(directory.user("U2").unwrap().label(), "Alice A");
        assert_eq!(directory.direct_with("U2").unwrap().id, "D1");
        let calls = fake.calls.lock().unwrap();
        assert!(calls[2]
            .1
            .contains(&("cursor".to_string(), "next".to_string())));
    }

    #[test]
    fn test_directory_cache_round_trip_and_expiry() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("directory.json");
        let directory = Directory::new(
            Workspace {
                team_id: "T1".into(),
                ..Default::default()
            },
            vec![User {
                id: "U1".into(),
                ..Default::default()
            }],
            Vec::new(),
            1_000,
        );
        write_directory_to(&path, &directory);
        let read = read_directory_from(&path, 1_000 + 60).unwrap();
        assert!(read.is_fresh(1_000 + 60));
        assert!(read.user("U1").is_some());
        assert!(!read.is_fresh(1_000 + 2 * 3600));
        assert!(read_directory_from(&path, 1_000 + 2 * 24 * 3600).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cache_belongs_to_one_account_and_expires() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-cache-{}", std::process::id()));
        let path = dir.join("sub").join("live.json");
        let account = |team: &str, user: &str| Workspace {
            team_id: team.into(),
            user_id: user.into(),
            ..Default::default()
        };
        let owner = account("T1", "U1");
        write_cache_to(&path, &owner, &vec!["C1".to_string()], 1_000);
        let read: Option<(Vec<String>, Duration)> = read_cache_from(&path, &owner, 1_060);
        assert_eq!(
            read,
            Some((vec!["C1".to_string()], Duration::from_secs(60)))
        );
        for other in [account("T1", "U2"), account("T2", "U1")] {
            assert!(read_cache_from::<Vec<String>>(&path, &other, 1_060).is_none());
        }
        assert!(read_cache_from::<Vec<String>>(&path, &owner, 1_000 + 2 * 24 * 3600).is_none());
        // Written before the workspace was recorded.
        std::fs::write(&path, r#"{"user":"U1","fetched_at":1000,"value":["C1"]}"#).unwrap();
        assert!(read_cache_from::<Vec<String>>(&path, &owner, 1_060).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_per_account_cache_paths_keep_offline_fallbacks_isolated() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-accounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let account_a = Workspace {
            team_id: "T1".into(),
            user_id: "U1".into(),
            ..Default::default()
        };
        let account_b = Workspace {
            team_id: "T2".into(),
            user_id: "U2".into(),
            ..Default::default()
        };
        assert!(account_cache_path_in(&dir, UNREAD_FILE, &Workspace::default()).is_none());

        for name in [UNREAD_FILE, THREADS_FILE, ACTIVITY_FILE] {
            let path_a = account_cache_path_in(&dir, name, &account_a).unwrap();
            let path_b = account_cache_path_in(&dir, name, &account_b).unwrap();
            assert_ne!(path_a, path_b);
            write_cache_to(&path_a, &account_a, &vec!["A".to_string()], 1_000);
            write_cache_to(&path_b, &account_b, &vec!["B".to_string()], 1_000);
            assert_eq!(
                read_cache_from::<Vec<String>>(&path_a, &account_a, 1_060).map(|(v, _)| v),
                Some(vec!["A".to_string()])
            );
            assert_eq!(
                read_cache_from::<Vec<String>>(&path_b, &account_b, 1_060).map(|(v, _)| v),
                Some(vec!["B".to_string()])
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_racing_writes_leave_one_whole_file() {
        let dir = std::env::temp_dir().join(format!("raffi-slack-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("unread.json");
        std::thread::scope(|scope| {
            for writer in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    let data = serde_json::to_vec(&vec![writer.to_string(); 2_000]).unwrap();
                    for _ in 0..20 {
                        write_private(path, &data);
                    }
                });
            }
        });
        let read: Vec<String> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(read.iter().all(|entry| entry == &read[0]));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp left");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_client_asks_whose_credentials_once_slack_answers() {
        let fake = FakeHttp::with(vec![
            ok(serde_json::json!({"ok": false, "error": "fatal_error"})),
            ok(serde_json::json!({"ok": true, "team_id": "T2", "user_id": "U2"})),
        ]);
        let client = Client::fake(Box::new(fake.clone()));
        assert!(
            client.identity().is_err() && client.known_identity().is_none(),
            "not known while Slack cannot say"
        );
        assert_eq!(client.identity().unwrap().user_id, "U2");
        assert_eq!(client.identity().unwrap().team_id, "T2");
        assert_eq!(fake.methods(), ["auth.test", "auth.test"], "asked once");
        assert!(client.known_identity().is_some());
    }

    #[test]
    fn test_learn_users_remembers_only_people_slack_says_are_missing() {
        let workspace = Workspace {
            team_id: "E1".into(),
            enterprise: true,
            ..Default::default()
        };
        let directory = Arc::new(Directory::new(workspace, Vec::new(), Vec::new(), 1_000));
        let fake = FakeHttp::with(vec![
            ok(serde_json::json!({"ok": false, "error": "fatal_error"})),
            ok(serde_json::json!({"ok": false, "error": "user_not_found"})),
        ]);
        let client = Client::fake(Box::new(fake.clone()));
        let ids = ["U8".to_string()];
        let learned = learn_users(&client, &directory, &ids);
        assert!(learned.missing.is_empty());
        let learned = learn_users(&client, &learned, &ids);
        assert_eq!(learned.missing, ["U8"]);
        // Kept with the directory, so it is not asked for again.
        let dir = std::env::temp_dir().join(format!("raffi-slack-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("directory.json");
        write_directory_to(&path, &learned);
        let read = Arc::new(read_directory_from(&path, 1_060).unwrap());
        learn_users(&client, &read, &ids);
        assert_eq!(fake.methods(), ["users.info", "users.info"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
