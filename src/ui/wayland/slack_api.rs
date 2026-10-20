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
pub(super) const LIVE_FILE: &str = "live.json";
pub(super) const UNREAD_FILE: &str = "unread.json";
pub(super) const MUTED_FILE: &str = "muted.json";
pub(super) const PREVIEW_DIR: &str = "previews";
const DIRECTORY_VERSION: u32 = 2;
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

pub(super) struct Client {
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
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
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
        Self {
            http,
            sleep: |_| {},
            kind,
            rejection: Mutex::new(None),
            identity: OnceLock::new(),
        }
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

/// The client of this process, built from the user's commands on first use
/// so a password manager prompt only shows once.
static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);

pub(super) fn client(
    token_command: Option<&str>,
    cookie_command: Option<&str>,
) -> Result<Arc<Client>, String> {
    let mut slot = CLIENT
        .lock()
        .map_err(|_| "Slack client unavailable".to_string())?;
    // A refused client is rebuilt so the commands print the updated token.
    if let Some(client) = slot.as_ref().filter(|client| client.rejection().is_none()) {
        return Ok(client.clone());
    }
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
    let client = Arc::new(Client::new(Credentials::new(token, cookie)?));
    *slot = Some(client.clone());
    Ok(client)
}

/// The client already built by a load, for actions run from the list.
pub(super) fn current_client() -> Result<Arc<Client>, String> {
    CLIENT
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or_else(|| "Slack is not connected".to_string())
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
    if raw.deleted || raw.is_bot || raw.id.is_empty() || raw.id == "USLACKBOT" {
        return None;
    }
    Some(User {
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
                .map(|id| scope.spawn(move || client.call("users.info", &[("user", id)])))
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
        store_directory(&learned);
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

fn directory_path() -> Option<PathBuf> {
    cache_path(DIRECTORY_FILE)
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
    if let Ok(data) = serde_json::to_vec(directory) {
        write_private(path, &data);
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
    if let Some((mut value, _)) = read_cache::<T>(name, workspace) {
        update(&mut value);
        write_cache(name, workspace, &value);
    }
}

/// The file `name` in the Slack cache directory; none in unit tests.
pub(super) fn cache_path(name: &str) -> Option<PathBuf> {
    crate::ui::get_slack_cache_dir()
        .ok()
        .map(|dir| dir.join(name))
}

/// The value cached as `name` for the account of `workspace` and its age.
pub(super) fn read_cache<T: DeserializeOwned>(
    name: &str,
    workspace: &Workspace,
) -> Option<(T, Duration)> {
    read_cache_from(&cache_path(name)?, workspace, now_secs())
}

pub(super) fn write_cache<T: Serialize>(name: &str, workspace: &Workspace, value: &T) {
    if let Some(path) = cache_path(name) {
        write_cache_to(&path, workspace, value, now_secs());
    }
}

/// The directory kept in memory once read or fetched in this process.
static DIRECTORY: Mutex<Option<Arc<Directory>>> = Mutex::new(None);

pub(super) fn cached_directory() -> Option<Arc<Directory>> {
    if let Some(directory) = DIRECTORY.lock().ok().and_then(|slot| slot.clone()) {
        return Some(directory);
    }
    let directory = Arc::new(read_directory_from(&directory_path()?, now_secs())?);
    if let Ok(mut slot) = DIRECTORY.lock() {
        *slot = Some(directory.clone());
    }
    Some(directory)
}

/// Bumped each time the credentials turn out to belong to another account
/// than the cached directory, so work started for the old one is dropped.
static ACCOUNT_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(super) fn account_epoch() -> u64 {
    ACCOUNT_EPOCH.load(Ordering::SeqCst)
}

/// The cached directory if it belongs to the account of `client`. One of
/// another account is forgotten at once, so it is never shown again. When
/// Slack cannot be asked, as offline, the cached one is kept; nothing is
/// cached until [`Client::known_identity`] is known.
pub(super) fn verified_directory(client: &Client) -> Result<Option<Arc<Directory>>, String> {
    let account = match client.identity() {
        Ok(account) => account,
        Err(error) if client.rejection().is_some() => return Err(error),
        Err(error) => {
            crate::debug_log!("slack: cannot tell whose the token is: {error}");
            return Ok(cached_directory());
        }
    };
    let Some(directory) = cached_directory() else {
        return Ok(None);
    };
    if directory.workspace.same_account(&account) {
        return Ok(Some(directory));
    }
    crate::debug_log!("slack: the token belongs to another account, forgetting the cache");
    forget_directory();
    Ok(None)
}

fn forget_directory() {
    if let Ok(mut slot) = DIRECTORY.lock() {
        *slot = None;
    }
    if let Some(path) = directory_path() {
        let _ = std::fs::remove_file(path);
    }
    ACCOUNT_EPOCH.fetch_add(1, Ordering::SeqCst);
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
    store_directory(&directory);
    Ok(directory)
}

fn store_directory(directory: &Arc<Directory>) {
    if let Some(path) = directory_path() {
        write_directory_to(&path, directory);
    }
    // Like the disk cache, kept out of unit tests so one test's directory
    // does not turn into another's cached rows.
    if cfg!(test) {
        return;
    }
    if let Ok(mut slot) = DIRECTORY.lock() {
        *slot = Some(directory.clone());
    }
}

/// Remove everything cached but the usage counts.
pub fn clear_cache() {
    if let Ok(dir) = crate::ui::get_slack_cache_dir() {
        for file in [DIRECTORY_FILE, LIVE_FILE, UNREAD_FILE, MUTED_FILE] {
            let _ = std::fs::remove_file(dir.join(file));
        }
        for subdir in ["avatars", PREVIEW_DIR] {
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
        assert_eq!(directory.users.len(), 1);
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
