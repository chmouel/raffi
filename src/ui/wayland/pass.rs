//! The `pass` (password-store) backend: lists entries without decrypting
//! them and decrypts with gpg directly, so Raffi can tell a missing
//! passphrase from a wrong one and ask for it itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::secret::Secret;
use crate::PassPinentry;

use super::command_list::CommandItem;
use super::secret_io::{run_checked, Cmd, Runner, SecretError};
use super::vault::{EntryAction, Field};

const AGENT_TIMEOUT: Duration = Duration::from_secs(120);
const SYNC_TIMEOUT: Duration = Duration::from_secs(60);
const ICON: &str = "dialog-password";

pub(super) const LOCK_IDENTITY: &str = "pass:lock";
pub(super) const SYNC_IDENTITY: &str = "pass:sync";
const ENTRY_PREFIX: &str = "pass:entry:";

#[derive(Debug, Clone)]
pub(super) struct PassContext {
    pub store_dir: PathBuf,
    pub pinentry: PassPinentry,
    pub username_fields: Vec<String>,
    pub gpg: String,
}

impl PassContext {
    pub fn new(config: &crate::PassAddonConfig) -> Self {
        Self {
            store_dir: resolve_store_dir(
                config.store_dir.as_deref(),
                std::env::var("PASSWORD_STORE_DIR").ok().as_deref(),
            ),
            pinentry: config.pinentry,
            username_fields: config.username_fields(),
            gpg: super::actions::find_tool(&["gpg2", "gpg"])
                .unwrap_or("gpg")
                .to_string(),
        }
    }

    fn pass(&self, args: &[&str]) -> Cmd {
        Cmd::new("pass", args).env("PASSWORD_STORE_DIR", &self.store_dir.to_string_lossy())
    }
}

/// Configured directory, else `$PASSWORD_STORE_DIR`, else
/// `~/.password-store`.
pub(super) fn resolve_store_dir(configured: Option<&str>, env: Option<&str>) -> PathBuf {
    match configured.or(env).filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(crate::expand_tilde(dir)),
        None => PathBuf::from(crate::expand_tilde("~/.password-store")),
    }
}

/// The row identity of entry `name`.
pub(super) fn entry_identity(name: &str) -> String {
    format!("{ENTRY_PREFIX}{name}")
}

/// The entry an identity from `load_entries` points at.
pub(super) fn entry_name(identity: &str) -> Option<&str> {
    identity.strip_prefix(ENTRY_PREFIX)
}

/// Entry rows, then Lock and, for a git store, Sync. Nothing is decrypted.
pub(super) fn load_entries(ctx: &PassContext) -> Result<Vec<CommandItem>, String> {
    let dir = &ctx.store_dir;
    if !dir.is_dir() {
        return Err(format!("No password store at {}", dir.display()));
    }
    let mut names: Vec<String> = walkdir::WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(dir).ok()?.to_str()?;
            relative.strip_suffix(".gpg").map(str::to_string)
        })
        .collect();
    names.sort();
    let mut items: Vec<CommandItem> = names.iter().map(|name| entry_item(name)).collect();
    items.push(command_item(
        LOCK_IDENTITY,
        "Lock password store",
        "Forget cached GPG passphrases (gpgconf --reload gpg-agent)",
        "system-lock-screen",
    ));
    if dir.join(".git").exists() {
        items.push(command_item(
            SYNC_IDENTITY,
            "Sync password store",
            "pass git pull --ff-only",
            "view-refresh",
        ));
    }
    Ok(items)
}

fn entry_item(name: &str) -> CommandItem {
    let (folder, _) = name.rsplit_once('/').unwrap_or(("", name));
    let mut details = vec![("Entry".to_string(), name.to_string())];
    if !folder.is_empty() {
        details.push(("Folder".to_string(), folder.to_string()));
    }
    CommandItem {
        title: name.to_string(),
        subtitle: folder.to_string(),
        match_text: name.to_string(),
        icon_names: vec![ICON.to_string()],
        icon: None,
        identity: entry_identity(name),
        unavailable: None,
        primary: None,
        secondary: None,
        details,
        badge: None,
        domains: entry_domains(name),
        dimmed: false,
    }
}

/// Sites an entry is named after: path parts such as `github.com` in
/// `web/github.com`, `github.com/alice` or `alice@github.com`.
fn entry_domains(name: &str) -> Vec<String> {
    super::command_list::domains_of(name.split('/'))
}

pub(super) fn command_item(identity: &str, title: &str, subtitle: &str, icon: &str) -> CommandItem {
    CommandItem {
        title: title.to_string(),
        subtitle: subtitle.to_string(),
        match_text: title.to_string(),
        icon_names: vec![icon.to_string()],
        icon: None,
        identity: identity.to_string(),
        unavailable: None,
        primary: None,
        secondary: None,
        details: Vec::new(),
        badge: None,
        domains: Vec::new(),
        dimmed: false,
    }
}

/// How gpg gets the passphrase.
#[derive(Debug, Clone, Copy)]
pub(super) enum Pinentry<'a> {
    /// Only what gpg-agent has cached; fails with `NeedPassphrase`.
    Cached,
    /// This passphrase, through loopback pinentry.
    Loopback(&'a Secret),
    /// gpg-agent's own pinentry.
    Agent,
}

fn decrypt_cmd(ctx: &PassContext, file: &Path, pinentry: Pinentry<'_>) -> Cmd {
    let mode = match pinentry {
        Pinentry::Cached => "error",
        Pinentry::Loopback(_) => "loopback",
        Pinentry::Agent => "ask",
    };
    let mut args = vec![
        "--batch",
        "--quiet",
        "--status-fd",
        "2",
        "--pinentry-mode",
        mode,
    ];
    if matches!(pinentry, Pinentry::Loopback(_)) {
        args.extend(["--passphrase-fd", "0"]);
    }
    args.extend(["--decrypt", "--"]);
    let file = file.to_string_lossy();
    args.push(&file);
    let cmd = Cmd::new(&ctx.gpg, &args);
    match pinentry {
        Pinentry::Loopback(passphrase) => {
            cmd.stdin(Secret::new(format!("{}\n", passphrase.expose())))
        }
        Pinentry::Agent => cmd.timeout(AGENT_TIMEOUT),
        Pinentry::Cached => cmd,
    }
}

/// Classify a failed decryption from gpg's status lines.
pub(super) fn classify_status(status: &str) -> SecretError {
    let mut error = SecretError::DecryptFailed;
    for line in status.lines() {
        let Some(line) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        let mut words = line.split_whitespace();
        match words.next() {
            Some("BAD_PASSPHRASE") => return SecretError::BadPassphrase,
            Some("SC_OP_FAILURE") => return SecretError::HardwareError,
            Some("ERROR") => {
                let _location = words.next();
                let Some(code) = words.next().and_then(|code| code.parse::<u32>().ok()) else {
                    continue;
                };
                match code & 0xFFFF {
                    // GPG_ERR_NO_PIN_ENTRY, GPG_ERR_NO_PASSPHRASE
                    85 | 177 => error = SecretError::NeedPassphrase,
                    // GPG_ERR_BAD_PASSPHRASE, GPG_ERR_BAD_PIN
                    11 | 87 => return SecretError::BadPassphrase,
                    // GPG_ERR_CANCELED
                    99 => return SecretError::Cancelled,
                    // GPG_ERR_CARD and friends, GPG_ERR_NO_CARD...
                    108 | 112 | 114 | 115 | 116 | 118 | 119 | 120 => {
                        return SecretError::HardwareError
                    }
                    // GPG_ERR_NO_SECKEY
                    17 if error == SecretError::DecryptFailed => error = SecretError::NoSecretKey,
                    _ => {}
                }
            }
            Some("NEED_PASSPHRASE") if error == SecretError::DecryptFailed => {
                error = SecretError::NeedPassphrase;
            }
            _ => {}
        }
    }
    error
}

/// Decrypt `entry`. Plaintext is only accepted when gpg says it decrypted
/// and exits successfully.
pub(super) fn decrypt(
    runner: &dyn Runner,
    ctx: &PassContext,
    entry: &str,
    pinentry: Pinentry<'_>,
) -> Result<Secret, SecretError> {
    let file = ctx.store_dir.join(format!("{entry}.gpg"));
    if !file.is_file() {
        return Err(SecretError::NotFound);
    }
    let output = runner.run(&decrypt_cmd(ctx, &file, pinentry))?;
    let status = String::from_utf8_lossy(output.stderr.expose());
    let decrypted = status
        .lines()
        .any(|line| line.trim_end() == "[GNUPG:] DECRYPTION_OKAY");
    if output.success() && decrypted {
        return String::from_utf8(output.stdout.expose().to_vec())
            .map(Secret::new)
            .map_err(|_| SecretError::Tool("Entry is not text".into()));
    }
    Err(classify_status(&status))
}

/// The password: the first line.
pub(super) fn password(plain: &Secret) -> Secret {
    let first = plain.expose().lines().next().unwrap_or("");
    Secret::new(first.to_string())
}

/// The value of the first of `fields` (case-insensitive, in that order) that
/// the entry has as a `key: value` line, else the entry's file name, as in
/// the common `site/user` layout.
pub(super) fn username(plain: &Secret, entry: &str, fields: &[String]) -> Secret {
    let lines: Vec<(&str, &str)> = plain
        .expose()
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect();
    for field in fields {
        if let Some((_, value)) = lines
            .iter()
            .find(|(key, value)| key.eq_ignore_ascii_case(field) && !value.is_empty())
        {
            return Secret::new(value.to_string());
        }
    }
    Secret::new(entry.rsplit('/').next().unwrap_or(entry).to_string())
}

/// The current code from `pass otp`, which decrypts again on its own; the
/// caller decrypted first so the passphrase is cached.
fn otp(runner: &dyn Runner, ctx: &PassContext, entry: &str) -> Result<Secret, SecretError> {
    let output = runner.run(&ctx.pass(&["otp", "code", "--", entry]))?;
    if output.success() {
        return output
            .stdout
            .into_secret_line()
            .filter(|code| !code.is_empty())
            .ok_or(SecretError::OtpFailed);
    }
    let stderr = String::from_utf8_lossy(output.stderr.expose());
    if stderr.contains("otp is not in the password store") {
        Err(SecretError::OtpUnsupported)
    } else {
        Err(SecretError::OtpFailed)
    }
}

/// Decrypt `entry` and pick what `action` needs.
pub(super) fn fetch(
    runner: &dyn Runner,
    ctx: &PassContext,
    entry: &str,
    action: EntryAction,
    pinentry: Pinentry<'_>,
) -> Result<Secret, SecretError> {
    let plain = decrypt(runner, ctx, entry, pinentry)?;
    let password = || {
        let password = password(&plain);
        if password.is_empty() {
            Err(SecretError::Tool("Entry has no password".into()))
        } else {
            Ok(password)
        }
    };
    match action {
        EntryAction::Copy(Field::Password) | EntryAction::Type => password(),
        EntryAction::Copy(Field::Username) => Ok(username(&plain, entry, &ctx.username_fields)),
        EntryAction::Copy(Field::Otp) => {
            if !plain.expose().contains("otpauth://") {
                return Err(SecretError::NoOtp);
            }
            drop(plain);
            otp(runner, ctx, entry)
        }
    }
}

/// Forget every passphrase gpg-agent has cached.
pub(super) fn lock(runner: &dyn Runner) -> Result<(), SecretError> {
    run_checked(runner, &Cmd::new("gpgconf", &["--reload", "gpg-agent"])).map(|_| ())
}

/// Fast-forward the store from its git remote; anything else is left to
/// the user.
pub(super) fn sync(runner: &dyn Runner, ctx: &PassContext) -> Result<(), SecretError> {
    let cmd = ctx
        .pass(&["git", "pull", "--ff-only"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .timeout(SYNC_TIMEOUT);
    run_checked(runner, &cmd).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretBytes;
    use std::fs;
    use std::sync::Mutex;

    use super::super::secret_io::Output;

    /// Answers each command with the next canned output and records it.
    struct FakeRunner {
        outputs: Mutex<Vec<Output>>,
        seen: Mutex<Vec<Cmd>>,
    }

    impl FakeRunner {
        fn new(outputs: Vec<(i32, &str, &str)>) -> Self {
            Self {
                outputs: Mutex::new(
                    outputs
                        .into_iter()
                        .rev()
                        .map(|(code, stdout, stderr)| Output {
                            code: Some(code),
                            stdout: SecretBytes::new(stdout.as_bytes().to_vec()),
                            stderr: SecretBytes::new(stderr.as_bytes().to_vec()),
                        })
                        .collect(),
                ),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output, SecretError> {
            self.seen.lock().unwrap().push(cmd.clone());
            self.outputs
                .lock()
                .unwrap()
                .pop()
                .ok_or(SecretError::Tool("unexpected command".into()))
        }
    }

    fn store(name: &str, entries: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "raffi-pass-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for entry in entries {
            let path = dir.join(entry);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"").unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn context(dir: &Path) -> PassContext {
        PassContext {
            store_dir: dir.to_path_buf(),
            pinentry: PassPinentry::Raffi,
            username_fields: vec!["login".into(), "user".into()],
            gpg: "gpg".into(),
        }
    }

    const OKAY: &str = "[GNUPG:] DECRYPTION_OKAY\n";

    #[test]
    fn test_entry_domains_from_the_path() {
        assert_eq!(entry_domains("web/login.github.com"), ["github.com"]);
        assert_eq!(entry_domains("github.com/alice"), ["github.com"]);
        assert_eq!(entry_domains("mail/alice@example.co.uk"), ["example.co.uk"]);
        assert!(entry_domains("bank/savings").is_empty());
    }

    #[test]
    fn test_resolve_store_dir_prefers_config_then_env() {
        assert_eq!(
            resolve_store_dir(Some("/a"), Some("/b")),
            PathBuf::from("/a")
        );
        assert_eq!(resolve_store_dir(None, Some("/b")), PathBuf::from("/b"));
        assert!(resolve_store_dir(None, None).ends_with(".password-store"));
    }

    #[test]
    fn test_load_entries_lists_gpg_files_and_skips_dot_dirs() {
        let dir = store(
            "list",
            &[
                "web/github.com.gpg",
                "mail.gpg",
                ".git/x.gpg",
                ".gpg-id",
                "notes.txt",
            ],
        );
        let items = load_entries(&context(&dir)).unwrap();
        let ids: Vec<&str> = items.iter().map(|item| item.identity.as_str()).collect();
        assert_eq!(
            ids,
            [
                "pass:entry:mail",
                "pass:entry:web/github.com",
                LOCK_IDENTITY,
                SYNC_IDENTITY
            ]
        );
        assert_eq!(items[1].subtitle, "web");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_classify_status_codes() {
        let status = |code: u32| {
            format!("[GNUPG:] NO_SECKEY ABC\n[GNUPG:] ERROR pkdecrypt_failed {code}\n[GNUPG:] DECRYPTION_FAILED\n")
        };
        assert_eq!(
            classify_status(&status(67108949)),
            SecretError::NeedPassphrase
        );
        assert_eq!(
            classify_status(&status(67108875)),
            SecretError::BadPassphrase
        );
        assert_eq!(classify_status(&status(33554449)), SecretError::NoSecretKey);
        assert_eq!(classify_status(&status(83886179)), SecretError::Cancelled);
        assert_eq!(
            classify_status(&status(100663404)),
            SecretError::HardwareError
        );
        assert_eq!(
            classify_status("[GNUPG:] DECRYPTION_FAILED\n"),
            SecretError::DecryptFailed
        );
        assert_eq!(
            classify_status("[GNUPG:] BAD_PASSPHRASE ABC\n"),
            SecretError::BadPassphrase
        );
    }

    #[test]
    fn test_decrypt_uses_loopback_with_passphrase_on_stdin_only() {
        let dir = store("loopback", &["site.gpg"]);
        let runner = FakeRunner::new(vec![(0, "hunter2\nlogin: me\n", OKAY)]);
        let passphrase = Secret::new("correct horse".into());
        let plain = decrypt(
            &runner,
            &context(&dir),
            "site",
            Pinentry::Loopback(&passphrase),
        )
        .unwrap();
        assert_eq!(plain.expose(), "hunter2\nlogin: me\n");
        let seen = runner.seen.lock().unwrap();
        let cmd = &seen[0];
        assert!(cmd
            .args
            .windows(2)
            .any(|pair| pair == ["--pinentry-mode", "loopback"]));
        assert!(cmd
            .args
            .windows(2)
            .any(|pair| pair == ["--passphrase-fd", "0"]));
        assert!(!cmd.args.iter().any(|arg| arg.contains("correct horse")));
        assert!(cmd
            .env
            .iter()
            .all(|(_, value)| !value.contains("correct horse")));
        assert_eq!(cmd.stdin.as_ref().unwrap().expose(), "correct horse\n");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_decrypt_needs_okay_status_and_success() {
        let dir = store("okay", &["site.gpg"]);
        let ctx = context(&dir);
        let runner = FakeRunner::new(vec![(0, "leak", ""), (2, "leak", OKAY)]);
        assert_eq!(
            decrypt(&runner, &ctx, "site", Pinentry::Cached).unwrap_err(),
            SecretError::DecryptFailed
        );
        assert_eq!(
            decrypt(&runner, &ctx, "site", Pinentry::Cached).unwrap_err(),
            SecretError::DecryptFailed
        );
        assert_eq!(
            decrypt(&runner, &ctx, "missing", Pinentry::Cached).unwrap_err(),
            SecretError::NotFound
        );
        let seen = runner.seen.lock().unwrap();
        assert!(seen[0]
            .args
            .windows(2)
            .any(|pair| pair == ["--pinentry-mode", "error"]));
        assert!(seen[0].stdin.is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_username_from_fields_or_entry_name() {
        let fields = vec!["login".to_string(), "user".to_string()];
        let plain = Secret::new("pw\nURL: x\nUser: bob\nlogin:  alice \n".into());
        assert_eq!(username(&plain, "web/site", &fields).expose(), "alice");
        let plain = Secret::new("pw\nurl: x\n".into());
        assert_eq!(
            username(&plain, "web/site/carol", &fields).expose(),
            "carol"
        );
        // The password line never counts as a field.
        let plain = Secret::new("login: notme\n".into());
        assert_eq!(username(&plain, "dave", &fields).expose(), "dave");
    }

    #[test]
    fn test_fetch_otp_checks_entry_then_runs_pass_otp() {
        let dir = store("otp", &["site.gpg"]);
        let ctx = context(&dir);
        let action = EntryAction::Copy(Field::Otp);
        let runner = FakeRunner::new(vec![(0, "pw\n", OKAY)]);
        assert_eq!(
            fetch(&runner, &ctx, "site", action, Pinentry::Cached).unwrap_err(),
            SecretError::NoOtp
        );

        let plain = "pw\notpauth://totp/x?secret=ABC\n";
        let runner = FakeRunner::new(vec![(0, plain, OKAY), (0, "123456\n", "")]);
        let code = fetch(&runner, &ctx, "site", action, Pinentry::Cached).unwrap();
        assert_eq!(code.expose(), "123456");
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen[1].program, "pass");
        assert_eq!(seen[1].args, ["otp", "code", "--", "site"]);
        assert!(seen[1]
            .env
            .contains(&("PASSWORD_STORE_DIR".into(), dir.to_string_lossy().into())));
        drop(seen);

        let runner = FakeRunner::new(vec![
            (0, plain, OKAY),
            (1, "", "Error: otp is not in the password store.\n"),
        ]);
        assert_eq!(
            fetch(&runner, &ctx, "site", action, Pinentry::Cached).unwrap_err(),
            SecretError::OtpUnsupported
        );
        fs::remove_dir_all(dir).unwrap();
    }

    /// Real gpg with a throwaway home and key: no cached passphrase means
    /// NeedPassphrase, a wrong one BadPassphrase, the right one decrypts.
    #[test]
    fn test_decrypt_with_real_gpg() {
        if super::super::actions::find_tool(&["gpg"]).is_none() {
            return;
        }
        let dir = store("realgpg", &[]);
        let home = dir.join("gnupg");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let gpg = |args: &[&str], input: Option<&str>| {
            use std::io::Write;
            let mut child = std::process::Command::new("gpg")
                .args(["--homedir", home.to_str().unwrap(), "--batch", "--quiet"])
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(input.unwrap_or("").as_bytes()).unwrap();
            drop(stdin);
            child.wait().unwrap().success()
        };
        let created = gpg(
            &[
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "right",
                "--quick-generate-key",
                "raffi-test@example.invalid",
                "default",
                "default",
                "never",
            ],
            None,
        );
        let encrypted = created
            && gpg(
                &[
                    "--trust-model",
                    "always",
                    "-r",
                    "raffi-test@example.invalid",
                    "-o",
                    dir.join("site.gpg").to_str().unwrap(),
                    "--encrypt",
                ],
                Some("pw\nlogin: me\n"),
            );
        if !encrypted {
            let _ = std::process::Command::new("gpgconf")
                .args(["--homedir", home.to_str().unwrap(), "--kill", "gpg-agent"])
                .status();
            fs::remove_dir_all(dir).unwrap();
            return;
        }
        let mut ctx = context(&dir);
        ctx.gpg = "gpg".into();
        let runner = HomeRunner(home.clone());
        assert_eq!(
            decrypt(&runner, &ctx, "site", Pinentry::Cached).unwrap_err(),
            SecretError::NeedPassphrase
        );
        let wrong = Secret::new("wrong".into());
        assert_eq!(
            decrypt(&runner, &ctx, "site", Pinentry::Loopback(&wrong)).unwrap_err(),
            SecretError::BadPassphrase
        );
        let right = Secret::new("right".into());
        let plain = decrypt(&runner, &ctx, "site", Pinentry::Loopback(&right)).unwrap();
        assert_eq!(password(&plain).expose(), "pw");
        // gpg-agent now has it cached.
        assert!(decrypt(&runner, &ctx, "site", Pinentry::Cached).is_ok());
        let _ = std::process::Command::new("gpgconf")
            .args(["--homedir", home.to_str().unwrap(), "--kill", "gpg-agent"])
            .status();
        fs::remove_dir_all(dir).unwrap();
    }

    /// The real runner, with GNUPGHOME pointing at a test home.
    struct HomeRunner(PathBuf);

    impl Runner for HomeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output, SecretError> {
            let cmd = cmd.clone().env("GNUPGHOME", &self.0.to_string_lossy());
            super::super::secret_io::run_split(&cmd, &Default::default())
        }
    }
}
