//! Running commands for the password manager addons without leaking the
//! secrets they handle: secrets go to children on stdin (or, where the
//! upstream tool offers nothing else, in the child's environment), output is
//! captured as `SecretBytes`, and errors carry fixed messages only.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::secret::{Secret, SecretBytes};

use super::actions::{find_tool, kill_process_group, program_name};

const POLL: Duration = Duration::from_millis(25);
const MAX_STDERR: usize = 64 * 1024;
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(1);
const PASTE_TIMEOUT: Duration = Duration::from_millis(500);
const TYPE_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a secret operation failed. Every variant maps to a fixed message that
/// never includes secrets or raw command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SecretError {
    /// gpg needs the passphrase and none is cached.
    NeedPassphrase,
    BadPassphrase,
    Unauthenticated,
    NoSecretKey,
    DecryptFailed,
    Cancelled,
    HardwareError,
    OtpUnsupported,
    NoOtp,
    OtpFailed,
    NotFound,
    Tool(String),
}

impl SecretError {
    pub fn message(&self) -> String {
        match self {
            Self::NeedPassphrase => "GPG passphrase needed".into(),
            Self::BadPassphrase => "Bad passphrase".into(),
            Self::Unauthenticated => {
                "rbw is not set up: run `rbw config set email` and `rbw login` in a terminal".into()
            }
            Self::NoSecretKey => "No secret key to decrypt this entry".into(),
            Self::DecryptFailed => "Decryption failed".into(),
            Self::Cancelled => "Cancelled".into(),
            Self::HardwareError => "Smartcard or security key error".into(),
            Self::OtpUnsupported => "pass otp is not installed (pass-otp extension)".into(),
            Self::NoOtp => "No OTP configured for this entry".into(),
            Self::OtpFailed => "Cannot generate the OTP code".into(),
            Self::NotFound => "Entry not found".into(),
            Self::Tool(message) => message.clone(),
        }
    }
}

extern "C" {
    fn setsid() -> i32;
}

/// Start the child in a session of its own, so it outlives Raffi and the
/// terminal it may have been started from.
fn detach(command: &mut Command) {
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        command.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
}

/// A command run for a secret operation. Arguments never hold secrets.
#[derive(Debug, Clone)]
pub(super) struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Secret>,
    pub timeout: Duration,
    pub max_output: usize,
}

impl Cmd {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(30),
            max_output: 1024 * 1024,
        }
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    pub fn stdin(mut self, input: Secret) -> Self {
        self.stdin = Some(input);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn max_output(mut self, max_output: usize) -> Self {
        self.max_output = max_output;
        self
    }

    fn name(&self) -> &str {
        program_name(&self.program)
    }
}

#[derive(Debug)]
pub(super) struct Output {
    pub code: Option<i32>,
    pub stdout: SecretBytes,
    pub stderr: SecretBytes,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs commands; faked in tests.
pub(super) trait Runner: Send + Sync {
    fn run(&self, cmd: &Cmd) -> Result<Output, SecretError>;

    /// Whether the operation using this runner was dropped.
    fn cancelled(&self) -> bool {
        false
    }
}

/// Kills the children of one operation when it is dropped: the process
/// groups registered so far right away, later ones as soon as they start.
#[derive(Debug, Clone, Default)]
pub(super) struct Cancel(Arc<Mutex<CancelState>>);

#[derive(Debug, Default)]
struct CancelState {
    cancelled: bool,
    groups: HashSet<u32>,
}

impl Cancel {
    pub fn cancel(&self) {
        let groups: Vec<u32> = match self.0.lock() {
            Ok(mut state) => {
                state.cancelled = true;
                state.groups.drain().collect()
            }
            Err(_) => return,
        };
        for group in groups {
            kill_process_group(group);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().map(|state| state.cancelled).unwrap_or(true)
    }

    /// Track `group`; false when already cancelled, and the caller must
    /// kill it.
    fn register(&self, group: u32) -> bool {
        match self.0.lock() {
            Ok(mut state) if !state.cancelled => {
                state.groups.insert(group);
                true
            }
            _ => false,
        }
    }

    fn unregister(&self, group: u32) {
        if let Ok(mut state) = self.0.lock() {
            state.groups.remove(&group);
        }
    }
}

pub(super) struct SystemRunner {
    pub cancel: Cancel,
}

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output, SecretError> {
        run_split(cmd, &self.cancel)
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

enum Captured {
    Done(Vec<u8>),
    Overflow,
    Failed,
}

fn read_all(reader: &mut impl Read, max: usize) -> Captured {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Captured::Done(output),
            Ok(count) => {
                output.extend_from_slice(&chunk[..count]);
                if output.len() > max {
                    return Captured::Overflow;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Captured::Failed,
        }
    }
}

fn feed_stdin(child: &mut Child, input: Option<&Secret>) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    let bytes = input.map(|secret| secret.expose().as_bytes().to_vec());
    std::thread::spawn(move || {
        if let Some(bytes) = bytes {
            let _ = stdin.write_all(&bytes);
        }
    });
}

/// Run `cmd` in its own process group, with its stdout and stderr captured
/// separately. One deadline covers the whole run; on timeout, overflow or
/// cancellation the group is killed and the child reaped.
pub(super) fn run_split(cmd: &Cmd, cancel: &Cancel) -> Result<Output, SecretError> {
    let name = cmd.name().to_string();
    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .stdin(if cmd.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (key, value) in &cmd.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| {
        SecretError::Tool(if error.kind() == std::io::ErrorKind::NotFound {
            format!("{name} not found")
        } else {
            format!("cannot start {name}")
        })
    })?;
    let group = child.id();
    let fail = |child: &mut Child, error: SecretError| {
        kill_process_group(group);
        let _ = child.wait();
        cancel.unregister(group);
        Err(error)
    };
    if !cancel.register(group) {
        return fail(&mut child, SecretError::Cancelled);
    }
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return fail(&mut child, SecretError::Tool(format!("cannot read {name}")));
    };
    feed_stdin(&mut child, cmd.stdin.as_ref());
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let max_output = cmd.max_output;
    std::thread::spawn(move || {
        let _ = stdout_sender.send((true, read_all(&mut stdout, max_output)));
    });
    std::thread::spawn(move || {
        let _ = sender.send((false, read_all(&mut stderr, MAX_STDERR)));
    });

    let deadline = Instant::now() + cmd.timeout;
    let timed_out = || SecretError::Tool(format!("{name} timed out"));
    let (mut out, mut err) = (None, None);
    while out.is_none() || err.is_none() {
        if cancel.is_cancelled() {
            return fail(&mut child, SecretError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return fail(&mut child, timed_out());
        }
        match receiver.recv_timeout(remaining.min(POLL)) {
            Ok((is_stdout, Captured::Done(bytes))) => {
                if is_stdout {
                    out = Some(bytes);
                } else {
                    err = Some(bytes);
                }
            }
            Ok((_, Captured::Overflow)) => {
                return fail(
                    &mut child,
                    SecretError::Tool(format!("{name} output too large")),
                )
            }
            Ok((_, Captured::Failed)) => {
                return fail(&mut child, SecretError::Tool(format!("cannot read {name}")))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return fail(&mut child, SecretError::Tool(format!("cannot read {name}")))
            }
        }
    }
    let status = loop {
        if cancel.is_cancelled() {
            return fail(&mut child, SecretError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => return fail(&mut child, timed_out()),
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                return fail(
                    &mut child,
                    SecretError::Tool(format!("cannot wait for {name}")),
                )
            }
        }
    };
    cancel.unregister(group);
    Ok(Output {
        code: status.code(),
        stdout: SecretBytes::new(out.unwrap_or_default()),
        stderr: SecretBytes::new(err.unwrap_or_default()),
    })
}

/// Run `cmd` and fail with a fixed message unless it exits successfully.
pub(super) fn run_checked(runner: &dyn Runner, cmd: &Cmd) -> Result<Output, SecretError> {
    let output = runner.run(cmd)?;
    if output.success() {
        Ok(output)
    } else {
        Err(SecretError::Tool(match output.code {
            Some(code) => format!("{} failed (exit {code})", cmd.name()),
            None => format!("{} was killed", cmd.name()),
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeTool {
    Wtype,
    Ydotool,
}

impl TypeTool {
    pub fn detect() -> Result<Self, SecretError> {
        match find_tool(&["wtype", "ydotool"]) {
            Some("ydotool") => Ok(Self::Ydotool),
            Some(_) => Ok(Self::Wtype),
            None => Err(SecretError::Tool(
                "cannot type text: install wtype or ydotool".into(),
            )),
        }
    }

    fn type_cmd(self, text: &Secret) -> Cmd {
        match self {
            Self::Wtype => Cmd::new("wtype", &["-"]),
            Self::Ydotool => Cmd::new("ydotool", &["type", "--file", "-"]),
        }
        .stdin(text.clone())
        .timeout(TYPE_TIMEOUT)
    }
}

pub(super) fn type_secret(
    runner: &dyn Runner,
    tool: TypeTool,
    text: &Secret,
) -> Result<(), SecretError> {
    run_checked(runner, &tool.type_cmd(text)).map(|_| ())
}

/// Commands that put text on the clipboard and read it back.
#[derive(Debug, Clone)]
pub(super) struct ClipboardTools {
    /// Copies stdin and keeps serving it in the foreground until it loses
    /// the selection or is killed.
    pub serve: Vec<String>,
    pub paste: Vec<String>,
    /// `timeout` for clearing the copy; required when clearing is wanted.
    pub timeout: Option<String>,
}

impl ClipboardTools {
    pub fn detect() -> Result<Self, SecretError> {
        let tool = find_tool(&["wl-copy", "xclip", "xsel"]).ok_or_else(|| {
            SecretError::Tool("cannot copy: install wl-clipboard, xclip or xsel".into())
        })?;
        let strings = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect();
        let (serve, paste) = match tool {
            "xclip" => (
                strings(&["xclip", "-quiet", "-selection", "clipboard"]),
                strings(&["xclip", "-o", "-selection", "clipboard"]),
            ),
            "xsel" => (
                strings(&["xsel", "--nodetach", "--clipboard", "--input"]),
                strings(&["xsel", "-o", "--clipboard"]),
            ),
            _ => {
                let mut serve: Vec<String> = strings(&["wl-copy", "--foreground"]);
                if wl_copy_has_sensitive() {
                    serve.push("--sensitive".into());
                }
                (serve, strings(&["wl-paste", "-n"]))
            }
        };
        Ok(Self {
            serve,
            paste,
            timeout: find_tool(&["timeout"]).map(String::from),
        })
    }
}

/// Whether this wl-copy can mark a copy as sensitive for clipboard
/// managers.
fn wl_copy_has_sensitive() -> bool {
    let cmd = Cmd::new("wl-copy", &["--help"]).timeout(Duration::from_secs(2));
    run_split(&cmd, &Cancel::default()).is_ok_and(|output| {
        let mentions =
            |bytes: &SecretBytes| String::from_utf8_lossy(bytes.expose()).contains("--sensitive");
        mentions(&output.stdout) || mentions(&output.stderr)
    })
}

/// Copy `secret` and confirm it by reading the clipboard back. With
/// `clear_after` seconds, the copy tool serves the selection under
/// `timeout`, so the copy ends then, but only if it is still ours: the tool
/// exits on its own when something else is copied.
pub(super) fn copy_secret(
    tools: &ClipboardTools,
    secret: &Secret,
    clear_after: u64,
) -> Result<(), SecretError> {
    let mut argv: Vec<String> = Vec::new();
    if clear_after > 0 {
        let timeout = tools.timeout.as_ref().ok_or_else(|| {
            SecretError::Tool("timeout not found: cannot clear the clipboard later".into())
        })?;
        argv.extend([
            timeout.clone(),
            "--kill-after=2s".into(),
            format!("{clear_after}s"),
        ]);
    }
    argv.extend(tools.serve.iter().cloned());
    let name = program_name(&tools.serve[0]).to_string();
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    let mut child = command.spawn().map_err(|error| {
        SecretError::Tool(if error.kind() == std::io::ErrorKind::NotFound {
            format!("{} not found", program_name(&argv[0]))
        } else {
            format!("cannot start {name}")
        })
    })?;
    let group = child.id();
    feed_stdin(&mut child, Some(secret));

    let deadline = Instant::now() + CONFIRM_TIMEOUT;
    let paste = Cmd::new(&tools.paste[0], &[])
        .timeout(PASTE_TIMEOUT)
        .max_output(secret.expose().len() + 1024);
    let paste = Cmd {
        args: tools.paste[1..].to_vec(),
        ..paste
    };
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return Err(SecretError::Tool(format!("{name} failed")));
            }
        }
        let copied = run_split(&paste, &Cancel::default()).is_ok_and(|output| {
            output.success() && output.stdout.expose() == secret.expose().as_bytes()
        });
        if copied {
            return Ok(());
        }
        if Instant::now() >= deadline {
            kill_process_group(group);
            let _ = child.wait();
            return Err(SecretError::Tool("copy not confirmed".into()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "raffi-secret-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn run_split_keeps_streams_apart_and_feeds_stdin() {
        let cmd = Cmd::new("sh", &["-c", "cat; echo err >&2; exit 3"])
            .stdin(Secret::new("hunter2".into()));
        let output = run_split(&cmd, &Cancel::default()).unwrap();
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout.expose(), b"hunter2");
        assert_eq!(output.stderr.expose(), b"err\n");
        assert!(!format!("{output:?}").contains("hunter2"));
    }

    #[test]
    fn cancel_kills_a_running_command() {
        let cancel = Cancel::default();
        let canceller = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });
        let started = Instant::now();
        let cmd = Cmd::new("sleep", &["10"]);
        assert_eq!(
            run_split(&cmd, &cancel).unwrap_err(),
            SecretError::Cancelled
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run_split(&cmd, &cancel).unwrap_err(),
            SecretError::Cancelled
        );
    }

    /// Records each command; the secret must only ever reach stdin.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Cmd>>);

    impl Runner for Recorder {
        fn run(&self, cmd: &Cmd) -> Result<Output, SecretError> {
            self.0.lock().unwrap().push(cmd.clone());
            Ok(Output {
                code: Some(0),
                stdout: SecretBytes::default(),
                stderr: SecretBytes::default(),
            })
        }
    }

    #[test]
    fn type_secret_goes_through_stdin() {
        let password = Secret::new("hunter2".into());
        for (tool, args) in [
            (TypeTool::Wtype, vec!["-"]),
            (TypeTool::Ydotool, vec!["type", "--file", "-"]),
        ] {
            let recorder = Recorder::default();
            type_secret(&recorder, tool, &password).unwrap();
            let commands = recorder.0.lock().unwrap();
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].args, args);
            assert_eq!(commands[0].stdin.as_ref().unwrap().expose(), "hunter2");
            assert!(commands[0].env.is_empty());
        }
    }

    /// Fake clipboard: `serve` stores stdin and owns the selection until
    /// another copy takes it or it is killed, clearing it only if still the
    /// owner, like wl-copy --foreground.
    fn fake_clipboard(dir: &Path) -> ClipboardTools {
        let clip = dir.join("clip");
        let owner = dir.join("owner");
        let serve = script(
            dir,
            "serve",
            &format!(
                "id=$$\ncat > '{clip}.new'\nmv '{clip}.new' '{clip}'\necho $id > '{owner}'\n\
                 trap '[ \"$(cat {owner})\" = \"$id\" ] && : > {clip}; exit 0' TERM\n\
                 while [ \"$(cat '{owner}')\" = \"$id\" ]; do sleep 0.05; done",
                clip = clip.display(),
                owner = owner.display()
            ),
        );
        let paste = script(dir, "paste", &format!("cat '{}'", clip.display()));
        ClipboardTools {
            serve: vec![serve],
            paste: vec![paste],
            timeout: find_tool(&["timeout"]).map(String::from),
        }
    }

    #[test]
    fn copy_expires_only_our_own_selection() {
        let dir = temp_dir("clip");
        let tools = fake_clipboard(&dir);
        let secret = Secret::new("hunter2".into());
        copy_secret(&tools, &secret, 1).unwrap();
        assert_eq!(fs::read_to_string(dir.join("clip")).unwrap(), "hunter2");
        std::thread::sleep(Duration::from_millis(1600));
        assert_eq!(fs::read_to_string(dir.join("clip")).unwrap(), "");

        copy_secret(&tools, &secret, 1).unwrap();
        fs::write(dir.join("clip"), "newer").unwrap();
        fs::write(dir.join("owner"), "someone else").unwrap();
        std::thread::sleep(Duration::from_millis(1600));
        assert_eq!(fs::read_to_string(dir.join("clip")).unwrap(), "newer");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_reports_failures() {
        let dir = temp_dir("clip-fail");
        let secret = Secret::new("hunter2".into());
        let mut tools = fake_clipboard(&dir);
        tools.serve = vec![dir.join("missing").to_string_lossy().into_owned()];
        assert!(copy_secret(&tools, &secret, 0).is_err());

        tools.serve = vec![script(&dir, "fails", "cat >/dev/null; exit 1")];
        assert_eq!(
            copy_secret(&tools, &secret, 0).unwrap_err(),
            SecretError::Tool("fails failed".into())
        );

        tools.serve = vec![script(&dir, "never", "cat >/dev/null; sleep 30")];
        let started = Instant::now();
        assert_eq!(
            copy_secret(&tools, &secret, 5).unwrap_err(),
            SecretError::Tool("copy not confirmed".into())
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        tools.timeout = None;
        assert!(copy_secret(&tools, &secret, 5).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
