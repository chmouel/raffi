//! The Bitwarden backend, through rbw. rbw's agent keeps the vault
//! unlocked and asks for the master password with its own pinentry, so
//! Raffi never sees it. `rbw list --raw` holds no secrets; they are fetched
//! one item at a time with `rbw get --raw`, whose id is checked because rbw
//! falls back to matching names when no item has the id asked for.

use std::collections::HashSet;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::IgnoredAny;
use serde::Deserialize;

use crate::secret::Secret;

use super::command_list::{domains_of, CommandItem};
use super::pass::command_item;
use super::secret_io::{Cmd, Runner, SecretError};
use super::vault::{EntryAction, Field};

const RBW: &str = "rbw";
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// Pinentry never times out on its own; Escape cancels before this.
const UNLOCK_TIMEOUT: Duration = Duration::from_secs(600);
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);
const LIST_MAX_OUTPUT: usize = 64 * 1024 * 1024;
/// Filters this short match too much to be worth a notes search.
pub(super) const SEARCH_MIN_CHARS: usize = 3;
/// Pill on rows found only by the notes search.
pub(super) const NOTES_BADGE: &str = "notes";
/// Pause in typing before searching notes.
pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
const ICON: &str = "bitwarden";
const FALLBACK_ICON: &str = "dialog-password";

pub(super) const LOCK_IDENTITY: &str = "bw:lock";
pub(super) const SYNC_IDENTITY: &str = "bw:sync";
const ITEM_PREFIX: &str = "bw:item:";

fn rbw(args: &[&str]) -> Cmd {
    Cmd::new(RBW, args)
}

fn stderr_text(output: &super::secret_io::Output) -> String {
    String::from_utf8_lossy(output.stderr.expose()).to_lowercase()
}

/// The row identity of item `id`.
pub(super) fn item_identity(id: &str) -> String {
    format!("{ITEM_PREFIX}{id}")
}

/// The item an identity from `load_items` points at.
pub(super) fn item_id(identity: &str) -> Option<&str> {
    identity.strip_prefix(ITEM_PREFIX)
}

/// Whether rbw's agent holds the unlocked vault. Never prompts.
pub(super) fn is_unlocked(runner: &dyn Runner) -> Result<bool, SecretError> {
    Ok(runner.run(&rbw(&["unlocked"]))?.success())
}

/// `rbw unlock`: rbw's pinentry asks for the master password, and logs in
/// first when needed.
pub(super) fn unlock(runner: &dyn Runner) -> Result<(), SecretError> {
    let output = runner.run(&rbw(&["unlock"]).timeout(UNLOCK_TIMEOUT))?;
    if output.success() {
        return Ok(());
    }
    let stderr = stderr_text(&output);
    Err(if stderr.contains("pinentry cancelled") {
        SecretError::Tool("Unlock cancelled".into())
    } else if stderr.contains("email address not set")
        || stderr.contains("must configure the email")
        || stderr.contains("failed to load config")
    {
        SecretError::Unauthenticated
    } else if stderr.contains("rbw register") {
        SecretError::Tool("Run `rbw register` in a terminal first".into())
    } else if stderr.contains("incorrect") || stderr.contains("invalid password") {
        SecretError::Tool("Wrong master password".into())
    } else {
        SecretError::Tool("Cannot unlock the vault with rbw".into())
    })
}

/// Only what the rows show; `rbw list --raw` has no secrets anyway.
#[derive(Deserialize)]
struct EntryJson {
    id: String,
    name: Option<String>,
    user: Option<String>,
    folder: Option<String>,
    uris: Option<Vec<String>>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Rows for the logins in `list`, with an OTP pill on the ids in `otp`.
pub(super) fn parse_items(
    list: &[u8],
    otp: &HashSet<String>,
) -> Result<Vec<CommandItem>, SecretError> {
    let entries: Vec<EntryJson> = serde_json::from_slice(list)
        .map_err(|_| SecretError::Tool("Unexpected output from rbw".into()))?;
    let non_empty = |value: Option<String>| value.filter(|value| !value.is_empty());
    let mut rows: Vec<CommandItem> = entries
        .into_iter()
        .filter(|entry| entry.kind.as_deref() == Some("Login"))
        .map(|entry| {
            let name = non_empty(entry.name).unwrap_or_else(|| "(no name)".into());
            let username = non_empty(entry.user);
            let folder = non_empty(entry.folder);
            let uris: Vec<String> = entry
                .uris
                .unwrap_or_default()
                .into_iter()
                .filter(|uri| !uri.is_empty())
                .collect();
            let uri = uris.first().cloned();
            let subtitle = [username.as_deref(), folder.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let match_text = [Some(name.as_str()), username.as_deref()]
                .into_iter()
                .flatten()
                .chain(uris.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let details = [("Username", &username), ("Folder", &folder), ("URL", &uri)]
                .into_iter()
                .filter_map(|(label, value)| Some((label.to_string(), value.clone()?)))
                .collect();
            CommandItem {
                title: name,
                subtitle,
                match_text,
                icon_names: vec![ICON.to_string(), FALLBACK_ICON.to_string()],
                icon: None,
                identity: item_identity(&entry.id),
                unavailable: None,
                primary: None,
                secondary: None,
                details,
                badge: otp.contains(&entry.id).then(|| "OTP".to_string()),
                domains: domains_of(uris.iter().map(String::as_str)),
                dimmed: false,
                sort_priority: 0,
                typed_only: false,
                close: None,
                group: None,
                pinned: false,
            }
        })
        .collect();
    rows.sort_by_key(|row| row.title.to_lowercase());
    Ok(rows)
}

/// rbw's config file, for the server and email that name its cache.
#[derive(Deserialize)]
struct RbwConfig {
    email: Option<String>,
    base_url: Option<String>,
}

/// rbw's local copy of the vault, as rbw names it (`dirs::db_file`), or
/// None when rbw is not configured.
fn cache_file(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let profile = match env("RBW_PROFILE").filter(|profile| !profile.is_empty()) {
        Some(profile) => format!("rbw-{profile}"),
        None => "rbw".to_string(),
    };
    // The `directories` crate lowercases the name and drops whitespace.
    let profile: String = profile.split_whitespace().map(str::to_lowercase).collect();
    let base = |xdg: &str, fallback: &str| {
        env(xdg)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(fallback)))
            .map(|dir| dir.join(&profile))
    };
    let config = std::fs::read(base("XDG_CONFIG_HOME", ".config")?.join("config.json")).ok()?;
    let config: RbwConfig = serde_json::from_slice(&config).ok()?;
    let email = config.email.filter(|email| !email.is_empty())?;
    let server = config.base_url.unwrap_or_else(|| "default".to_string());
    // percent_encoding's CONTROLS plus '/', '%' and ':'; non-ASCII bytes are
    // always encoded.
    let server: String = server
        .bytes()
        .map(|byte| match byte {
            b'/' | b'%' | b':' | 0..=0x1f | 0x7f.. => format!("%{byte:02X}"),
            _ => char::from(byte).to_string(),
        })
        .collect();
    Some(base("XDG_CACHE_HOME", ".cache")?.join(format!("{server}:{email}.json")))
}

#[derive(Deserialize)]
struct CacheJson {
    entries: Vec<CacheEntry>,
}

#[derive(Deserialize)]
struct CacheEntry {
    id: String,
    data: CacheData,
}

#[derive(Deserialize)]
struct CacheData {
    #[serde(rename = "Login")]
    login: Option<CacheLogin>,
}

#[derive(Deserialize)]
struct CacheLogin {
    totp: Option<IgnoredAny>,
}

/// Ids of the items with a TOTP secret. `rbw list` doesn't say, but its
/// cache keeps each secret encrypted, or null when there is none, so this
/// decrypts nothing. Streaming skips the tokens and other fields of the file
/// instead of keeping them. Empty when the format is not the expected one.
fn otp_ids(cache: impl std::io::Read) -> HashSet<String> {
    serde_json::from_reader::<_, CacheJson>(cache)
        .map(|cache| {
            cache
                .entries
                .into_iter()
                .filter(|entry| {
                    entry
                        .data
                        .login
                        .as_ref()
                        .is_some_and(|login| login.totp.is_some())
                })
                .map(|entry| entry.id)
                .collect()
        })
        .unwrap_or_default()
}

/// Login items, then Lock and Sync rows.
pub(super) fn load_items(runner: &dyn Runner) -> Result<Vec<CommandItem>, SecretError> {
    let output = runner.run(
        &rbw(&["list", "--raw"])
            .timeout(LIST_TIMEOUT)
            .max_output(LIST_MAX_OUTPUT),
    )?;
    if !output.success() {
        return Err(SecretError::Tool("Cannot list the vault".into()));
    }
    let otp = cache_file(|key| std::env::var(key).ok())
        .and_then(|path| std::fs::File::open(path).ok())
        .map(|file| otp_ids(BufReader::new(file)))
        .unwrap_or_default();
    let mut rows = parse_items(output.stdout.expose(), &otp)?;
    rows.push(command_item(
        LOCK_IDENTITY,
        "Lock vault",
        "rbw lock",
        "system-lock-screen",
    ));
    rows.push(command_item(
        SYNC_IDENTITY,
        "Sync vault",
        "rbw sync",
        "view-refresh",
    ));
    Ok(rows)
}

/// Identities of the logins whose notes, URIs or visible custom fields
/// contain `term`, as `rbw search` finds them. Only ids come back; the
/// notes stay in rbw. Empty when the vault is locked, so a search never
/// asks for the master password.
pub(super) fn search(runner: &dyn Runner, term: &str) -> Result<HashSet<String>, SecretError> {
    if !is_unlocked(runner)? {
        return Ok(HashSet::new());
    }
    let output = runner.run(
        &rbw(&["search", "--raw", "--", term])
            .timeout(LIST_TIMEOUT)
            .max_output(LIST_MAX_OUTPUT),
    )?;
    if !output.success() {
        return Err(SecretError::Tool("Cannot search the vault".into()));
    }
    let entries: Vec<EntryJson> = serde_json::from_slice(output.stdout.expose())
        .map_err(|_| SecretError::Tool("Unexpected output from rbw".into()))?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.kind.as_deref() == Some("Login"))
        .map(|entry| item_identity(&entry.id))
        .collect())
}

/// The parts of `rbw get --raw` Raffi uses; serde skips the rest, such as
/// notes and password history.
#[derive(Deserialize)]
struct ItemJson {
    id: String,
    data: LoginJson,
}

#[derive(Deserialize)]
struct LoginJson {
    username: Option<String>,
    password: Option<String>,
    totp: Option<String>,
}

struct Login {
    username: Option<Secret>,
    password: Option<Secret>,
    has_totp: bool,
}

fn secret(value: Option<String>) -> Option<Secret> {
    value.filter(|value| !value.is_empty()).map(Secret::new)
}

/// Item `id`, or `NotFound` when rbw answers with another item.
fn read_item(runner: &dyn Runner, id: &str) -> Result<Login, SecretError> {
    let output = runner.run(&rbw(&["get", "--raw", id]))?;
    if !output.success() {
        return Err(if stderr_text(&output).contains("couldn't find entry") {
            SecretError::NotFound
        } else {
            SecretError::Tool("Cannot read the item".into())
        });
    }
    let item: ItemJson = serde_json::from_slice(output.stdout.expose())
        .map_err(|_| SecretError::Tool("Cannot read the item".into()))?;
    if !item.id.eq_ignore_ascii_case(id) {
        return Err(SecretError::NotFound);
    }
    Ok(Login {
        username: secret(item.data.username),
        password: secret(item.data.password),
        has_totp: item.data.totp.is_some_and(|totp| !totp.is_empty()),
    })
}

fn code(runner: &dyn Runner, id: &str) -> Result<Secret, SecretError> {
    let output = runner.run(&rbw(&["code", id]))?;
    let stderr = stderr_text(&output);
    if output.success() {
        if let Some(code) = output
            .stdout
            .into_secret_line()
            .filter(|code| !code.is_empty())
        {
            return Ok(code);
        }
    }
    Err(if stderr.contains("couldn't find entry") {
        SecretError::NotFound
    } else if stderr.contains("does not contain a totp") {
        SecretError::NoOtp
    } else {
        SecretError::OtpFailed
    })
}

/// What `action` needs from item `id`.
pub(super) fn fetch(
    runner: &dyn Runner,
    id: &str,
    action: EntryAction,
) -> Result<Secret, SecretError> {
    let item = read_item(runner, id)?;
    let username = || {
        item.username
            .clone()
            .ok_or_else(|| SecretError::Tool("Item has no username".into()))
    };
    let password = || {
        item.password
            .clone()
            .ok_or_else(|| SecretError::Tool("Item has no password".into()))
    };
    match action {
        EntryAction::Copy(Field::Password) | EntryAction::Type => password(),
        EntryAction::Copy(Field::Username) => username(),
        // `rbw code` has no id-only mode; the id was checked just before.
        EntryAction::Copy(Field::Otp) if item.has_totp => code(runner, id),
        EntryAction::Copy(Field::Otp) => Err(SecretError::NoOtp),
    }
}

pub(super) fn sync(runner: &dyn Runner) -> Result<(), SecretError> {
    if runner.run(&rbw(&["sync"]).timeout(SYNC_TIMEOUT))?.success() {
        Ok(())
    } else {
        Err(SecretError::Tool("Sync failed".into()))
    }
}

pub(super) fn lock(runner: &dyn Runner) -> Result<(), SecretError> {
    if runner.run(&rbw(&["lock"]))?.success() {
        Ok(())
    } else {
        Err(SecretError::Tool("Cannot lock the vault".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretBytes;
    use std::sync::Mutex;

    use super::super::secret_io::Output;

    struct FakeRunner {
        outputs: Mutex<Vec<(i32, String, String)>>,
        seen: Mutex<Vec<Cmd>>,
    }

    impl FakeRunner {
        fn new(outputs: &[(i32, &str, &str)]) -> Self {
            Self {
                outputs: Mutex::new(
                    outputs
                        .iter()
                        .rev()
                        .map(|(code, out, err)| (*code, out.to_string(), err.to_string()))
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn argv(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|cmd| format!("{} {}", cmd.program, cmd.args.join(" ")))
                .collect()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output, SecretError> {
            self.seen.lock().unwrap().push(cmd.clone());
            let (code, out, err) = self
                .outputs
                .lock()
                .unwrap()
                .pop()
                .ok_or(SecretError::Tool("unexpected command".into()))?;
            Ok(Output {
                code: Some(code),
                stdout: SecretBytes::new(out.into_bytes()),
                stderr: SecretBytes::new(err.into_bytes()),
            })
        }
    }

    const LIST: &str = r#"[
        {"id":"1","name":"GitHub","user":"me","folder":"Work",
         "uris":["https://github.com","androidapp://com.github.android","https://api.github.com"],
         "type":"Login"},
        {"id":"2","name":"a note","user":null,"folder":null,"uris":null,"type":"Note"},
        {"id":"3","name":"bank","user":null,"folder":null,"uris":[],"type":"Login"}
    ]"#;

    #[test]
    fn test_parse_items_keeps_logins() {
        let otp = HashSet::from(["1".to_string()]);
        let rows = parse_items(LIST.as_bytes(), &otp).unwrap();
        let titles: Vec<_> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["bank", "GitHub"]);
        assert_eq!(rows[1].subtitle, "me · Work");
        assert_eq!(rows[1].identity, "bw:item:1");
        assert_eq!(
            rows[1].match_text,
            "GitHub me https://github.com androidapp://com.github.android https://api.github.com"
        );
        assert_eq!(rows[1].domains, ["github.com"]);
        assert!(rows[0].domains.is_empty());
        assert_eq!(rows[0].subtitle, "");
        assert_eq!(rows[1].badge.as_deref(), Some("OTP"));
        assert_eq!(rows[0].badge, None);
        assert!(parse_items(b"garbage", &otp).is_err());
    }

    #[test]
    fn test_otp_ids_from_rbw_cache() {
        let cache = r#"{"access_token":"tok","refresh_token":"tok","entries":[
            {"id":"a","name":"2.x","data":{"Login":{"username":null,"password":"2.p",
             "totp":"2.secret","uris":[]}},"fields":[],"history":[]},
            {"id":"b","data":{"Login":{"username":"2.u","password":"2.p","totp":null,"uris":[]}}},
            {"id":"c","data":{"Card":{"cardholder_name":null,"number":"2.n"}}}
        ]}"#;
        assert_eq!(otp_ids(cache.as_bytes()), HashSet::from(["a".to_string()]));
        assert!(otp_ids(&b"{not json"[..]).is_empty());
    }

    #[test]
    fn test_cache_file_follows_rbw_naming() {
        let dir = std::env::temp_dir().join(format!("raffi-rbw-{}", std::process::id()));
        let config = dir.join("config/rbw-work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            r#"{"email":"me@example.com","base_url":"https://vault.example.com/","pinentry":"pinentry"}"#,
        )
        .unwrap();
        let root = dir.to_string_lossy().to_string();
        let env = |key: &str| match key {
            "RBW_PROFILE" => Some("Work".to_string()),
            "XDG_CONFIG_HOME" => Some(format!("{root}/config")),
            "XDG_CACHE_HOME" => Some(format!("{root}/cache")),
            _ => None,
        };
        assert_eq!(
            cache_file(env).unwrap(),
            dir.join("cache/rbw-work/https%3A%2F%2Fvault.example.com%2F:me@example.com.json")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_load_appends_lock_and_sync() {
        let runner = FakeRunner::new(&[(0, LIST, "")]);
        let rows = load_items(&runner).unwrap();
        let identities: Vec<_> = rows.iter().map(|row| row.identity.as_str()).collect();
        assert_eq!(
            identities,
            ["bw:item:3", "bw:item:1", LOCK_IDENTITY, SYNC_IDENTITY]
        );
        assert_eq!(runner.argv(), ["rbw list --raw"]);
    }

    #[test]
    fn test_search_returns_login_identities() {
        let runner = FakeRunner::new(&[(0, "", ""), (0, LIST, "")]);
        let found = search(&runner, "-recovery code").unwrap();
        assert_eq!(
            found,
            HashSet::from(["bw:item:1".to_string(), "bw:item:3".to_string()])
        );
        assert_eq!(
            runner.argv(),
            ["rbw unlocked", "rbw search --raw -- -recovery code"]
        );
    }

    #[test]
    fn test_search_never_unlocks() {
        let runner = FakeRunner::new(&[(1, "", "")]);
        assert!(search(&runner, "recovery").unwrap().is_empty());
        assert_eq!(runner.argv(), ["rbw unlocked"]);
    }

    #[test]
    fn test_unlock_failures_have_fixed_messages() {
        for (stderr, expected) in [
            (
                "rbw unlock: pinentry cancelled",
                SecretError::Tool("Unlock cancelled".into()),
            ),
            (
                "rbw unlock: email address not set",
                SecretError::Unauthenticated,
            ),
            // Verbatim from rbw 1.15 with no config file.
            (
                "ERROR: Before using rbw, you must configure the email address you would like to use",
                SecretError::Unauthenticated,
            ),
            (
                "rbw unlock: failed to load config from /x/rbw/config.json: No such file or directory",
                SecretError::Unauthenticated,
            ),
            (
                "rbw unlock: something else: secret-ish detail",
                SecretError::Tool("Cannot unlock the vault with rbw".into()),
            ),
        ] {
            let runner = FakeRunner::new(&[(1, "", stderr)]);
            assert_eq!(unlock(&runner).unwrap_err(), expected);
        }
        let runner = FakeRunner::new(&[(0, "", "")]);
        assert!(unlock(&runner).is_ok());
        assert_eq!(runner.argv(), ["rbw unlock"]);
    }

    fn item(id: &str, data: &str) -> String {
        format!(
            r#"{{"id":"{id}","folder":null,"name":"x","data":{data},"fields":[],"notes":"n","history":[]}}"#
        )
    }

    #[test]
    fn test_fetch_reads_one_checked_item() {
        let full = item(
            "id-1",
            r#"{"username":"me","password":"hunter2","totp":null,"uris":[]}"#,
        );
        let runner = FakeRunner::new(&[(0, &full, "")]);
        let username = fetch(&runner, "id-1", EntryAction::Copy(Field::Username)).unwrap();
        assert_eq!(username.expose(), "me");
        assert_eq!(runner.argv(), ["rbw get --raw id-1"]);
        assert!(runner
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|cmd| cmd.stdin.is_none() && cmd.env.is_empty()));

        let otp = item(
            "id-1",
            r#"{"username":null,"password":null,"totp":"otpauth://x","uris":null}"#,
        );
        let runner = FakeRunner::new(&[(0, &otp, ""), (0, "123456\n", "")]);
        assert!(fetch(&runner, "id-1", EntryAction::Copy(Field::Otp)).is_ok());
        assert_eq!(runner.argv(), ["rbw get --raw id-1", "rbw code id-1"]);
    }

    #[test]
    fn test_fetch_fails_closed_on_another_item() {
        // A deleted id makes rbw match names instead, here another item.
        let other = item(
            "id-2",
            r#"{"username":"me","password":"hunter2","totp":"t","uris":null}"#,
        );
        for action in [EntryAction::Type, EntryAction::Copy(Field::Otp)] {
            let runner = FakeRunner::new(&[(0, &other, "")]);
            assert_eq!(
                fetch(&runner, "id-1", action).unwrap_err(),
                SecretError::NotFound
            );
            assert_eq!(runner.argv(), ["rbw get --raw id-1"]);
        }
    }

    #[test]
    fn test_fetch_errors() {
        let empty = item(
            "id",
            r#"{"username":"","password":null,"totp":null,"uris":null}"#,
        );
        for (action, expected) in [
            (
                EntryAction::Copy(Field::Username),
                SecretError::Tool("Item has no username".into()),
            ),
            (
                EntryAction::Type,
                SecretError::Tool("Item has no password".into()),
            ),
            (EntryAction::Copy(Field::Otp), SecretError::NoOtp),
        ] {
            let runner = FakeRunner::new(&[(0, &empty, "")]);
            assert_eq!(fetch(&runner, "id", action).unwrap_err(), expected);
        }
        let runner = FakeRunner::new(&[(1, "", "rbw get: couldn't find entry for 'id'")]);
        assert_eq!(
            fetch(&runner, "id", EntryAction::Type).unwrap_err(),
            SecretError::NotFound
        );
        let runner = FakeRunner::new(&[(0, "{not json hunter2", "")]);
        assert_eq!(
            fetch(&runner, "id", EntryAction::Type).unwrap_err(),
            SecretError::Tool("Cannot read the item".into())
        );
    }
}
