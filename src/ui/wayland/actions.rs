use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use super::placeholders::MAX_VALUE;

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);
const IMAGE_CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(30);
const DEFERRED_TIMEOUT: Duration = Duration::from_secs(2);
/// Typing a key per character is slow and each press is a command argument, so
/// keep caret movement for `{cursor}` within a sane bound.
const MAX_CURSOR_MOVES: usize = 1000;

pub(super) fn find_tool(candidates: &[&'static str]) -> Option<&'static str> {
    std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            for candidate in candidates {
                if dir.join(candidate).is_file() {
                    return Some(*candidate);
                }
            }
        }
        None
    })
}

/// Shell command that types `$RAFFI_INSERT_VALUE` and then presses Left
/// `cursor_moves` times. wtype treats every argument after `--` as text, so
/// the key presses go through a second invocation.
fn insert_command(tool: &str, cursor_moves: usize) -> String {
    let mut command = if tool == "ydotool" {
        "ydotool type -- \"$RAFFI_INSERT_VALUE\"".to_string()
    } else {
        "wtype -- \"$RAFFI_INSERT_VALUE\"".to_string()
    };
    if cursor_moves > 0 {
        if tool == "ydotool" {
            command.push_str(" && ydotool key");
            // 105 is KEY_LEFT; each move is a press followed by a release.
            command.push_str(&" 105:1 105:0".repeat(cursor_moves));
        } else {
            command.push_str(" && wtype");
            command.push_str(&" -k Left".repeat(cursor_moves));
        }
    }
    format!("sleep 0.2 && {command}")
}

fn spawn_insert(value: &str, cursor_moves: usize) -> Result<(), String> {
    let tool =
        find_tool(&["wtype", "ydotool"]).ok_or("cannot type text: install wtype or ydotool")?;
    Command::new("sh")
        .arg("-c")
        .arg(insert_command(tool, cursor_moves))
        .env("RAFFI_INSERT_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot run {tool}: {error}"))
}

/// Copy a value to the clipboard using the first available tool.
/// Fallback chain: wl-copy -> xclip -> xsel.
fn try_copy(value: &str) -> Result<(), String> {
    let tool = find_tool(&["wl-copy", "xclip", "xsel"])
        .ok_or("cannot copy: install wl-clipboard, xclip or xsel")?;

    let copy_cmd = match tool {
        "xclip" => "printf '%s' \"$RAFFI_COPY_VALUE\" | xclip -selection clipboard".to_string(),
        "xsel" => "printf '%s' \"$RAFFI_COPY_VALUE\" | xsel --clipboard --input".to_string(),
        _ => "wl-copy -- \"$RAFFI_COPY_VALUE\"".to_string(),
    };
    Command::new("sh")
        .arg("-c")
        .arg(&copy_cmd)
        .env("RAFFI_COPY_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot run {tool}: {error}"))
}

/// Copy `text` and report whether the clipboard tool accepted it. The text
/// goes through stdin only; the foreground process has to exit successfully
/// within the clipboard timeout (wl-copy forks its owner before exiting).
pub(super) fn copy_text_checked(text: &str) -> Result<(), String> {
    let tool = find_tool(&["wl-copy", "xclip", "xsel"])
        .ok_or("cannot copy: install wl-clipboard, xclip or xsel")?;
    let argv: &[&str] = match tool {
        "xclip" => &["xclip", "-selection", "clipboard"],
        "xsel" => &["xsel", "--clipboard", "--input"],
        _ => &["wl-copy"],
    };
    copy_with(argv, text, CLIPBOARD_TIMEOUT)
}

pub(super) fn copy_image_checked(path: &Path, mime_type: &str) -> Result<(), String> {
    let tool = find_tool(&["wl-copy", "xclip"])
        .ok_or("cannot copy image: install wl-clipboard or xclip")?;
    let mut command = Command::new(tool);
    if tool == "xclip" {
        command
            .args(["-selection", "clipboard", "-t"])
            .arg(mime_type)
            .arg("-i");
    } else {
        command.arg("--type").arg(mime_type);
    }
    copy_file_to_clipboard_with(&mut command, path, IMAGE_CLIPBOARD_TIMEOUT)
}

#[cfg(test)]
pub(super) fn copy_text_with(argv: &[String], text: &str) -> Result<(), String> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    copy_with(&argv, text, CLIPBOARD_TIMEOUT)
}

fn copy_file_to_clipboard_with(
    command: &mut Command,
    path: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let input = std::fs::File::open(path).map_err(|error| format!("cannot read image: {error}"))?;
    let name = program_name(&command.get_program().to_string_lossy()).to_string();
    let mut child = command
        .stdin(Stdio::from(input))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start {name}: {error}"))?;
    let deadline = Instant::now() + timeout;
    let status = match wait_until(&mut child, deadline) {
        Ok(status) => status,
        Err(error) => return Err(format!("{name} {error}")),
    };
    if !status.success() {
        return Err(exit_error(&name, status));
    }
    Ok(())
}

fn copy_with(argv: &[&str], text: &str, timeout: Duration) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("no clipboard tool")?;
    let name = program_name(program);
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start {name}: {error}"))?;
    let deadline = Instant::now() + timeout;
    let written = spawn_writer(&mut child, text.as_bytes().to_vec());
    let status = match wait_until(&mut child, deadline) {
        Ok(status) => status,
        Err(error) => return Err(format!("{name} {error}")),
    };
    if !status.success() {
        return Err(exit_error(name, status));
    }
    match written.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("{name} did not read the text")),
        Err(_) => {
            kill_process_group(child.id());
            Err(format!("{name} timed out reading the text"))
        }
    }
}

pub(super) fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

fn exit_error(name: &str, status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("{name} failed (exit {code})"),
        None => format!("{name} failed (killed by a signal)"),
    }
}

/// Feed `input` to the child's stdin from a thread and close it; the receiver
/// gets whether everything was written. A child that stops reading early
/// only breaks the pipe.
fn spawn_writer(child: &mut Child, input: Vec<u8>) -> mpsc::Receiver<bool> {
    let (sender, receiver) = mpsc::channel();
    let stdin = child.stdin.take();
    std::thread::spawn(move || {
        let written = stdin.is_some_and(|mut stdin| stdin.write_all(&input).is_ok());
        let _ = sender.send(written);
    });
    receiver
}

/// Wait for the child to exit until `deadline`; on timeout or a wait error
/// kill its process group and reap it.
fn wait_until(child: &mut Child, deadline: Instant) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            outcome => {
                kill_process_group(child.id());
                let _ = child.wait();
                return Err(match outcome {
                    Err(_) => "could not be waited for".into(),
                    _ => "timed out".into(),
                });
            }
        }
    }
}

/// Something to run once the window has closed, so it acts on whatever had
/// focus before Raffi opened.
#[derive(Debug, Clone)]
pub(super) enum DeferredAction {
    HyprPaste(super::hyprland::Hyprland),
    Command(Vec<String>),
    /// Commands run in order, each to completion, stopping at the first
    /// failure.
    CommandBatch(Vec<Vec<String>>),
    /// Quit niri, ending the session.
    NiriQuit,
    /// Make sway exit, ending the session.
    SwayExit,
    Hypr(super::hyprland::HyprAction),
    /// Signal a process after checking it is still the one listed.
    Signal(super::kill::SignalTarget),
    /// `bluetoothctl` with these arguments, failing on the errors it prints.
    Bluetooth(Vec<String>),
    /// Copy or type a password manager secret.
    Secret(super::vault::SecretJob),
    /// Switch to or close a browser tab.
    BrowserTab(super::browser_tabs::TabAction),
    /// Point the browsers' native messaging manifests at Raffi; `force`
    /// replaces those of other hosts such as Vicinae.
    BrowserHostSetup {
        force: bool,
    },
    /// Open a command in a new tab of a running terminal, then raise the
    /// window of process `raise.0` titled `raise.1`. When the tab cannot be
    /// opened, `window` runs instead.
    TerminalTab {
        kind: crate::terminal::TabKind,
        tab: Vec<String>,
        window: Vec<String>,
        raise: Option<(u32, String)>,
    },
}

impl DeferredAction {
    pub(super) fn run(&self) -> Result<(), String> {
        match self {
            Self::HyprPaste(target) => target.paste(),
            Self::Command(argv) => run_detached(argv, DEFERRED_TIMEOUT),
            Self::CommandBatch(commands) => run_batch(commands, DEFERRED_TIMEOUT),
            Self::NiriQuit => super::windows::niri_quit(),
            Self::SwayExit => super::windows::sway_exit(),
            Self::Hypr(action) => action.run(),
            Self::Signal(target) => super::kill::send_signal(target),
            Self::Bluetooth(args) => super::bluetooth::run_action(args),
            Self::Secret(job) => job.run(),
            Self::BrowserTab(action) => super::browser_tabs::run(action),
            Self::BrowserHostSetup { force } => {
                crate::browser_host::install_current(*force).map(|_| ())
            }
            Self::TerminalTab {
                kind,
                tab,
                window,
                raise,
            } => open_in_terminal_tab(*kind, tab, window, raise.as_ref(), DEFERRED_TIMEOUT),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::HyprPaste(_) => "hypr_paste",
            Self::Command(_) | Self::CommandBatch(_) => "action",
            Self::NiriQuit => "niri",
            Self::SwayExit => "swaymsg",
            Self::Hypr(_) => "hyprctl",
            Self::Signal(_) => "kill",
            Self::Bluetooth(_) => "bluetoothctl",
            Self::Secret(_) => "secret",
            Self::BrowserTab(_) | Self::BrowserHostSetup { .. } => "browser tabs",
            Self::TerminalTab { .. } => "terminal",
        }
    }

    fn report(&self, error: &str) {
        match self {
            Self::HyprPaste(target) => target.notify_error(error),
            _ => super::hyprland::notify(&format!("Raffi action failed: {error}")),
        }
    }
}

/// Run the queued action, if any, exactly once. Failures are notified and
/// returned.
pub(super) fn run_deferred(slot: &Mutex<Option<DeferredAction>>) -> Result<(), String> {
    run_deferred_with(slot, DeferredAction::run, DeferredAction::report)
}

fn run_deferred_with(
    slot: &Mutex<Option<DeferredAction>>,
    run: impl FnOnce(&DeferredAction) -> Result<(), String>,
    report: impl FnOnce(&DeferredAction, &str),
) -> Result<(), String> {
    let Some(action) = slot
        .lock()
        .map_err(|_| "cannot read pending action".to_string())?
        .take()
    else {
        return Ok(());
    };
    run(&action).map_err(|error| {
        report(&action, &error);
        format!("{}: {error}", action.label())
    })
}

/// Open the command in a new terminal tab, or in `window` when the terminal
/// cannot take it. Once the tab was asked for, a failure may still have
/// opened it, so nothing else runs and the failure is reported: the command
/// must not run twice.
fn open_in_terminal_tab(
    kind: crate::terminal::TabKind,
    tab: &[String],
    window: &[String],
    raise: Option<&(u32, String)>,
    timeout: Duration,
) -> Result<(), String> {
    let Some(tab) = check_terminal_tab(kind, tab, timeout) else {
        crate::debug_log!(
            "terminal: {} cannot open a tab, opening a window",
            kind.name()
        );
        return run_detached(window, timeout);
    };
    run_batch(&[tab], timeout).map_err(|error| {
        format!(
            "cannot tell whether the {} tab opened: {error}",
            kind.name()
        )
    })?;
    if let Some((pid, title)) = raise {
        if let Err(error) = super::windows::raise_process_window(*pid, title) {
            crate::debug_log!("terminal: cannot raise the terminal window: {error}");
        }
    }
    Ok(())
}

/// Ask the terminal, read-only, whether it takes remote commands, and
/// return the `tab` command line to run then. WezTerm is also told where
/// the tab goes, as it fails when it cannot tell which window has focus.
fn check_terminal_tab(
    kind: crate::terminal::TabKind,
    tab: &[String],
    timeout: Duration,
) -> Option<Vec<String>> {
    let query = |argv: Option<Vec<String>>| {
        let argv = argv?;
        let (program, args) = argv.split_first()?;
        let mut command = Command::new(program);
        command.args(args);
        match run_bounded_status(command, timeout, 16 * 1024 * 1024) {
            Ok((status, output)) if status.success() => Some(output),
            _ => None,
        }
    };
    match kind {
        crate::terminal::TabKind::Kitty => {
            query(crate::terminal::kitty_check_argv(tab)).map(|_| tab.to_vec())
        }
        crate::terminal::TabKind::WezTerm => {
            let list = query(crate::terminal::wezterm_list_argv(tab))?;
            let clients = query(crate::terminal::wezterm_clients_argv(tab));
            crate::terminal::wezterm_spawn_argv(tab, clients.as_deref(), &list)
        }
    }
}

/// Start `argv` in its own process group and fail if it exits unsuccessfully
/// within `timeout`. A command still running then (a screen locker, an app)
/// is left alone.
fn run_detached(argv: &[String], timeout: Duration) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("no command to run")?;
    let name = program_name(program);
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start {name}: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(exit_error(name, status)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => return Ok(()),
            Err(_) => return Err(format!("{name} could not be waited for")),
        }
    }
}

/// Run `commands` in order, each to completion within `timeout`, stopping at
/// the first failure.
fn run_batch(commands: &[Vec<String>], timeout: Duration) -> Result<(), String> {
    commands.iter().try_for_each(|argv| {
        let (program, args) = argv.split_first().ok_or("no command to run")?;
        let name = program_name(program);
        let mut command = Command::new(program);
        command.args(args);
        let (status, _) = run_bounded_status(command, timeout, 64 * 1024)
            .map_err(|error| format!("{name}: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(exit_error(name, status))
        }
    })
}

fn spawn_custom_action(
    action: &str,
    value: &str,
    env: &HashMap<String, String>,
) -> Result<(), String> {
    let command = action.replace("{value}", value);
    crate::debug_log!("action: executing shell command={command:?}");
    Command::new("sh")
        .arg("-c")
        .arg(&command)
        .envs(env)
        .env("RAFFI_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot run action: {error}"))
}

/// Execute an action for a selected value.
/// Supported keywords:
/// - `insert`: type into the focused app via `wtype` or `ydotool`
/// - `copy`: copy to clipboard via the first available clipboard tool
/// - other: execute as `sh -c` after replacing `{value}` in the command string;
///   the value is also available as `$RAFFI_VALUE`
pub(super) fn execute_action(action: &str, value: &str) {
    crate::debug_log!("action: action={action:?} value={value:?}");
    let _ = dispatch(action, value, 0);
}

/// [`execute_action`] with extra environment for shell command actions, such
/// as a Script Filter item's `variables`.
pub(super) fn execute_action_with_env(action: &str, value: &str, env: &HashMap<String, String>) {
    crate::debug_log!("action: action={action:?} value={value:?}");
    let _ = match action {
        "insert" | "copy" | "hypr_paste" => dispatch(action, value, 0),
        _ => spawn_custom_action(action, value, env),
    };
}

/// Execute a file browser action. `open` hands the path to `xdg-open`; every
/// other value behaves like [`execute_action`].
pub(super) fn execute_file_action(action: &str, path: &str) {
    if action != "open" {
        return execute_action(action, path);
    }
    crate::debug_log!("action: xdg-open {path:?}");
    let _ = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Known file managers with the `XDG_CURRENT_DESKTOP` names that ship them.
/// The order is also the fallback preference when no desktop matches.
const FILE_MANAGERS: &[(&[&str], &str)] = &[
    (&["GNOME", "Budgie", "Unity"], "nautilus"),
    (&["KDE"], "dolphin"),
    (&["XFCE"], "thunar"),
    (&["X-Cinnamon", "Cinnamon"], "nemo"),
    (&["MATE"], "caja"),
    (&["LXQt"], "pcmanfm-qt"),
    (&["LXDE"], "pcmanfm"),
    (&["Pantheon"], "io.elementary.files"),
    (&["COSMIC"], "cosmic-files"),
    (&["Deepin", "DDE"], "dde-file-manager"),
    (&[], "spacefm"),
    (&[], "krusader"),
];

/// Pick the desktop's own file manager when installed, otherwise the first
/// installed one. `desktop` is the colon-separated `XDG_CURRENT_DESKTOP`.
fn detect_file_manager(
    desktop: &str,
    installed: impl Fn(&'static str) -> bool,
) -> Option<&'static str> {
    let desktops: Vec<&str> = desktop.split(':').map(str::trim).collect();
    let for_desktop = FILE_MANAGERS.iter().find(|(names, binary)| {
        names
            .iter()
            .any(|name| desktops.iter().any(|d| d.eq_ignore_ascii_case(name)))
            && installed(binary)
    });
    for_desktop
        .or_else(|| FILE_MANAGERS.iter().find(|(_, binary)| installed(binary)))
        .map(|(_, binary)| *binary)
}

/// Shell command and `$RAFFI_VALUE` opening `dir` in `manager`, selecting
/// `selected` when the manager supports `--select`. A command containing
/// `{value}` is used as is with the directory; otherwise the path is appended.
fn file_manager_command<'a>(
    manager: &str,
    dir: &'a str,
    selected: Option<&'a str>,
) -> (String, &'a str) {
    if manager.contains("{value}") {
        return (manager.to_string(), dir);
    }
    match selected {
        Some(path) if supports_select(manager) => {
            (format!("{manager} --select \"$RAFFI_VALUE\""), path)
        }
        _ => (format!("{manager} \"$RAFFI_VALUE\""), dir),
    }
}

/// File managers whose `--select` opens the parent folder with the item
/// selected. Only a bare program name or path qualifies, so custom commands
/// with their own arguments are left alone.
fn supports_select(manager: &str) -> bool {
    let program = manager.trim();
    !program.contains(char::is_whitespace)
        && matches!(
            std::path::Path::new(program)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("nautilus" | "dolphin" | "nemo" | "caja")
        )
}

/// Open `dir` in the configured or detected file manager, falling back to
/// `xdg-open`, with `selected` highlighted when the manager supports it.
pub(super) fn open_in_file_manager(dir: &str, selected: Option<&str>, configured: Option<&str>) {
    let manager = configured.unwrap_or_else(|| {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        detect_file_manager(&desktop, |name| find_tool(&[name]).is_some()).unwrap_or("xdg-open")
    });
    let (command, value) = file_manager_command(manager, dir, selected);
    let _ = spawn_custom_action(&command, value, &HashMap::new());
}

fn dispatch(action: &str, value: &str, cursor_moves: usize) -> Result<(), String> {
    match action {
        "insert" => spawn_insert(value, cursor_moves),
        "copy" => try_copy(value),
        "hypr_paste" => {
            Err("hypr_paste is only available for text snippets in the native UI".into())
        }
        _ => spawn_custom_action(action, value, &HashMap::new()),
    }
}

/// Run a snippet action, reporting anything that would stop the value from
/// reaching its destination. `cursor_moves` only applies to `insert`.
pub(super) fn execute_snippet_action(
    action: &str,
    value: &str,
    cursor_moves: Option<usize>,
) -> Result<(), String> {
    crate::debug_log!("action: snippet action={action:?} cursor={cursor_moves:?}");
    validate_snippet(value)?;
    let cursor_moves = if action == "insert" {
        let moves = cursor_moves.unwrap_or(0);
        if moves > MAX_CURSOR_MOVES {
            return Err(format!(
                "{{cursor}} is {moves} characters from the end, the limit is {MAX_CURSOR_MOVES}"
            ));
        }
        moves
    } else {
        0
    };
    dispatch(action, value, cursor_moves)
}

pub(super) fn validate_snippet(value: &str) -> Result<(), String> {
    if value.len() > MAX_VALUE {
        return Err(format!(
            "snippet is larger than {} KiB and cannot be passed to the action",
            MAX_VALUE / 1024
        ));
    }
    if value.contains('\0') {
        return Err("snippet contains a NUL byte and cannot be passed to the action".into());
    }
    Ok(())
}

enum ReadOutcome {
    Done(Vec<u8>),
    Overflow,
    Failed(String),
}

fn read_limited(reader: &mut impl Read, max_output: usize) -> ReadOutcome {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return ReadOutcome::Done(output),
            Ok(count) => {
                output.extend_from_slice(&chunk[..count]);
                if output.len() > max_output {
                    return ReadOutcome::Overflow;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return ReadOutcome::Failed(error.to_string()),
        }
    }
}

pub(super) fn kill_process_group(pgid: u32) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "kill -s TERM -- -{pgid} 2>/dev/null; sleep 0.1; kill -s KILL -- -{pgid} 2>/dev/null"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run `command` in its own process group and return its stdout.
///
/// The deadline covers both reading output and waiting for exit. On timeout
/// or when stdout exceeds `max_output` bytes, the whole process group is
/// killed so that background children cannot keep the pipe open.
pub(super) fn run_bounded(
    command: Command,
    timeout: Duration,
    max_output: usize,
) -> Result<Vec<u8>, String> {
    let (status, output) = run_bounded_status(command, timeout, max_output)?;
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(output)
}

/// `run_bounded` that also returns stdout when the command fails, with its
/// exit status.
pub(super) fn run_bounded_status(
    mut command: Command,
    timeout: Duration,
    max_output: usize,
) -> Result<(ExitStatus, Vec<u8>), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start: {error}"))?;
    let pgid = child.id();
    let deadline = Instant::now() + timeout;

    let Some(mut stdout) = child.stdout.take() else {
        kill_process_group(pgid);
        let _ = child.wait();
        return Err("cannot read output".into());
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read_limited(&mut stdout, max_output));
    });

    let fail = |child: &mut std::process::Child, message: String| {
        kill_process_group(pgid);
        let _ = child.wait();
        Err(message)
    };

    let output = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(ReadOutcome::Done(output)) => output,
        Ok(ReadOutcome::Overflow) => {
            return fail(
                &mut child,
                format!("output is larger than {} KiB", max_output / 1024),
            )
        }
        Ok(ReadOutcome::Failed(error)) => {
            return fail(&mut child, format!("cannot read output: {error}"))
        }
        Err(_) => {
            return fail(
                &mut child,
                format!("timed out after {}s", timeout.as_secs_f32()),
            )
        }
    };

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                return fail(
                    &mut child,
                    format!("timed out after {}s", timeout.as_secs_f32()),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return fail(&mut child, format!("cannot wait: {error}")),
        }
    };
    Ok((status, output))
}

/// Run `argv` in its own process group with `input` on stdin and return its
/// stdout.
///
/// stdin is fed from a writer thread while stdout and stderr are drained by
/// their own reader threads, so neither a large input nor a chatty stderr can
/// deadlock the child. stdout is capped at `max_output` bytes and stderr at
/// `MAX_STDERR`; stderr is only drained, never kept. One deadline covers the
/// whole run, and the child is killed and reaped on any failure. Errors name
/// the program but never include its output or input, since a child can
/// echo its input back.
pub(super) fn run_bounded_with_input(
    argv: &[&str],
    input: &[u8],
    timeout: Duration,
    max_output: usize,
) -> Result<Vec<u8>, String> {
    const MAX_STDERR: usize = 64 * 1024;
    let (program, args) = argv.split_first().ok_or("no command to run")?;
    let name = program_name(program);
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start {name}: {error}"))?;
    let pgid = child.id();
    let deadline = Instant::now() + timeout;
    let fail = |child: &mut Child, message: String| {
        kill_process_group(pgid);
        let _ = child.wait();
        Err(message)
    };

    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return fail(&mut child, format!("cannot read {name} output"));
    };
    let _ = spawn_writer(&mut child, input.to_vec());
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    std::thread::spawn(move || {
        let _ = stdout_sender.send((true, read_limited(&mut stdout, max_output)));
    });
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        let mut total = 0usize;
        let outcome = loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break ReadOutcome::Done(Vec::new()),
                Ok(count) => {
                    total += count;
                    if total > MAX_STDERR {
                        break ReadOutcome::Overflow;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => break ReadOutcome::Failed(error.to_string()),
            }
        };
        let _ = sender.send((false, outcome));
    });

    let mut output = None;
    let mut stderr_open = true;
    while output.is_none() || stderr_open {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok((true, ReadOutcome::Done(bytes))) => output = Some(bytes),
            Ok((false, ReadOutcome::Done(_))) => stderr_open = false,
            Ok((true, ReadOutcome::Overflow)) => {
                return fail(
                    &mut child,
                    format!("{name} output is larger than {} KiB", max_output / 1024),
                )
            }
            Ok((false, ReadOutcome::Overflow)) => {
                return fail(&mut child, format!("{name} wrote too much error output"))
            }
            Ok((_, ReadOutcome::Failed(_))) => {
                return fail(&mut child, format!("cannot read {name} output"))
            }
            Err(_) => return fail(&mut child, format!("{name} timed out")),
        }
    }

    let status = wait_until(&mut child, deadline).map_err(|error| format!("{name} {error}"))?;
    if !status.success() {
        return Err(exit_error(name, status));
    }
    Ok(output.unwrap_or_default())
}

/// Read the clipboard as text, trying wl-paste, xclip and xsel in turn.
pub(super) fn read_clipboard() -> Result<String, String> {
    let backends: [(&str, &[&str]); 3] = [
        ("wl-paste", &["--no-newline", "--type", "text"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    let mut errors = Vec::new();
    for (tool, args) in backends {
        let mut command = Command::new(tool);
        command.args(args);
        match run_bounded(command, CLIPBOARD_TIMEOUT, MAX_VALUE) {
            Ok(bytes) => {
                return String::from_utf8(bytes).map_err(|_| "clipboard is not text".to_string())
            }
            // Tools that are not installed only add noise to the error.
            Err(error) if error.starts_with("cannot start:") => {}
            Err(error) => {
                if tool == "wl-paste" {
                    if let Some(reason) = wayland_clipboard_without_text() {
                        return Err(reason);
                    }
                }
                errors.push(format!("{tool}: {error}"));
            }
        }
    }
    if errors.is_empty() {
        return Err("no clipboard tool found: install wl-clipboard, xclip or xsel".into());
    }
    Err(format!(
        "clipboard is empty or unavailable ({})",
        errors.join("; ")
    ))
}

/// Explain a failed `wl-paste --type text` when the clipboard is empty or
/// holds only non-text data such as an image.
fn wayland_clipboard_without_text() -> Option<String> {
    let mut command = Command::new("wl-paste");
    command.arg("--list-types");
    match run_bounded(command, CLIPBOARD_TIMEOUT, 64 * 1024) {
        Ok(output) => describe_non_text(&String::from_utf8_lossy(&output)),
        Err(_) => None,
    }
}

fn describe_non_text(types: &str) -> Option<String> {
    let types: Vec<&str> = types
        .lines()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .collect();
    if types.is_empty() {
        return Some("clipboard is empty".into());
    }
    let is_text = |kind: &&str| {
        kind.starts_with("text/") || matches!(*kind, "TEXT" | "STRING" | "UTF8_STRING")
    };
    if types.iter().any(is_text) {
        return None;
    }
    let shown: Vec<&str> = types.iter().copied().take(3).collect();
    Some(format!(
        "clipboard has no text (it holds {})",
        shown.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn test_detect_file_manager_prefers_the_desktop() {
        let all = |_: &str| true;
        assert_eq!(detect_file_manager("KDE", all), Some("dolphin"));
        assert_eq!(detect_file_manager("ubuntu:GNOME", all), Some("nautilus"));
        assert_eq!(detect_file_manager("xfce", all), Some("thunar"));
        // Unknown desktops (Hyprland, sway...) take the first installed one.
        assert_eq!(
            detect_file_manager("Hyprland", |name| name == "thunar" || name == "nemo"),
            Some("thunar")
        );
        // The desktop's manager must be installed to win.
        assert_eq!(
            detect_file_manager("KDE", |name| name == "pcmanfm"),
            Some("pcmanfm")
        );
        assert_eq!(detect_file_manager("KDE", |_| false), None);
    }

    #[test]
    fn test_file_manager_command_selects_when_supported() {
        let dir = "/home/u/src/";
        let file = Some("/home/u/src/main.rs");
        assert_eq!(
            file_manager_command("nautilus", dir, file),
            (
                "nautilus --select \"$RAFFI_VALUE\"".into(),
                "/home/u/src/main.rs"
            )
        );
        assert_eq!(
            file_manager_command("/usr/bin/dolphin", dir, file),
            (
                "/usr/bin/dolphin --select \"$RAFFI_VALUE\"".into(),
                "/home/u/src/main.rs"
            )
        );
        // Nothing selected, or no --select support: open the directory.
        assert_eq!(
            file_manager_command("nautilus", dir, None),
            ("nautilus \"$RAFFI_VALUE\"".into(), dir)
        );
        assert_eq!(
            file_manager_command("thunar", dir, file),
            ("thunar \"$RAFFI_VALUE\"".into(), dir)
        );
        assert_eq!(
            file_manager_command("nautilus --new-window", dir, file),
            ("nautilus --new-window \"$RAFFI_VALUE\"".into(), dir)
        );
        assert_eq!(
            file_manager_command("kitty -d {value} yazi", dir, file),
            ("kitty -d {value} yazi".into(), dir)
        );
    }

    #[test]
    fn test_insert_command_without_cursor() {
        assert_eq!(
            insert_command("wtype", 0),
            "sleep 0.2 && wtype -- \"$RAFFI_INSERT_VALUE\""
        );
        assert_eq!(
            insert_command("ydotool", 0),
            "sleep 0.2 && ydotool type -- \"$RAFFI_INSERT_VALUE\""
        );
    }

    #[test]
    fn test_insert_command_moves_cursor_in_a_separate_invocation() {
        assert_eq!(
            insert_command("wtype", 2),
            "sleep 0.2 && wtype -- \"$RAFFI_INSERT_VALUE\" && wtype -k Left -k Left"
        );
        assert_eq!(
            insert_command("ydotool", 2),
            "sleep 0.2 && ydotool type -- \"$RAFFI_INSERT_VALUE\" && ydotool key 105:1 105:0 105:1 105:0"
        );
    }

    #[test]
    fn test_snippet_action_rejects_values_that_cannot_be_passed() {
        let big = "x".repeat(MAX_VALUE + 1);
        assert!(execute_snippet_action("copy", &big, None)
            .unwrap_err()
            .contains("larger than"));
        assert!(execute_snippet_action("copy", "a\0b", None)
            .unwrap_err()
            .contains("NUL"));
        assert!(
            execute_snippet_action("insert", "abc", Some(MAX_CURSOR_MOVES + 1))
                .unwrap_err()
                .contains("{cursor}")
        );
    }

    #[test]
    fn test_custom_action_receives_hostile_value_through_env() {
        let dir = std::env::temp_dir().join(format!("raffi-action-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let value = "it's \"quoted\"\n$(touch pwned) `touch pwned2` ; echo hi";
        // Run inside `dir` so an injected `touch pwned` would land there.
        let action = format!(
            "cd '{}' && printf '%s' \"$RAFFI_VALUE\" > '{}'",
            dir.display(),
            out.display()
        );
        execute_snippet_action(&action, value, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&out).is_ok_and(|written| written == value) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), value);
        assert!(!dir.join("pwned").exists());
        assert!(!dir.join("pwned2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_bounded_returns_output() {
        let output = run_bounded(sh("printf 'hi\\n'"), Duration::from_secs(5), 1024).unwrap();
        assert_eq!(output, b"hi\n");
    }

    #[test]
    fn test_run_bounded_reports_nonzero_exit() {
        let error = run_bounded(sh("echo out; exit 3"), Duration::from_secs(5), 1024).unwrap_err();
        assert!(error.contains("exited"), "{error}");
    }

    #[test]
    fn test_run_bounded_times_out() {
        let started = Instant::now();
        let error = run_bounded(sh("sleep 10"), Duration::from_millis(200), 1024).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_kills_processes_ignoring_term() {
        let started = Instant::now();
        let error = run_bounded(
            sh("trap '' TERM; sleep 3 & wait"),
            Duration::from_millis(200),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_run_bounded_kills_on_output_overflow() {
        let started = Instant::now();
        let error = run_bounded(sh("yes"), Duration::from_secs(10), 4096).unwrap_err();
        assert!(error.contains("larger than"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_kills_descendants_holding_stdout() {
        let started = Instant::now();
        let error = run_bounded(
            sh("(sleep 10 &); echo parent done"),
            Duration::from_millis(300),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_reports_missing_program() {
        let error = run_bounded(
            Command::new("raffi-no-such-program"),
            Duration::from_secs(1),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("cannot start"), "{error}");
    }

    #[test]
    fn test_run_bounded_with_input_feeds_large_input() {
        let input = vec![b'x'; 1024 * 1024];
        let output =
            run_bounded_with_input(&["wc", "-c"], &input, Duration::from_secs(5), 1024).unwrap();
        assert_eq!(String::from_utf8_lossy(&output).trim(), "1048576");

        let output =
            run_bounded_with_input(&["cat"], &input, Duration::from_secs(5), 2 * 1024 * 1024)
                .unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn test_run_bounded_with_input_tolerates_child_exiting_early() {
        let input = vec![b'y'; 1024 * 1024];
        let output =
            run_bounded_with_input(&["head", "-c1"], &input, Duration::from_secs(5), 1024).unwrap();
        assert_eq!(output, b"y");
    }

    #[test]
    fn test_run_bounded_with_input_errors_are_generic() {
        let error = run_bounded_with_input(
            &["sh", "-c", "cat >&2; cat; exit 1"],
            b"secret",
            Duration::from_secs(5),
            1024,
        )
        .unwrap_err();
        assert_eq!(error, "sh failed (exit 1)");

        let error = run_bounded_with_input(
            &["raffi-no-such-program"],
            b"secret",
            Duration::from_secs(1),
            1024,
        )
        .unwrap_err();
        assert!(
            error.starts_with("cannot start raffi-no-such-program"),
            "{error}"
        );
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn test_run_bounded_with_input_times_out() {
        let started = Instant::now();
        let error = run_bounded_with_input(&["sleep", "5"], b"", Duration::from_millis(200), 1024)
            .unwrap_err();
        assert_eq!(error, "sleep timed out");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn test_run_bounded_with_input_stops_stdout_overflow() {
        let started = Instant::now();
        let error =
            run_bounded_with_input(&["yes"], b"", Duration::from_secs(10), 4096).unwrap_err();
        assert!(error.contains("larger than"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_with_input_survives_stderr_flood() {
        // A child that writes a lot to stderr before stdout must not block.
        let output = run_bounded_with_input(
            &["sh", "-c", "head -c 60000 /dev/zero >&2; cat"],
            b"done",
            Duration::from_secs(5),
            1024,
        )
        .unwrap();
        assert_eq!(output, b"done");

        let started = Instant::now();
        let error =
            run_bounded_with_input(&["sh", "-c", "yes >&2"], b"", Duration::from_secs(10), 1024)
                .unwrap_err();
        assert_eq!(error, "sh wrote too much error output");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_copy_with_checks_the_copier() {
        copy_with(
            &["sh", "-c", "test \"$(cat)\" = \"two words\""],
            "two words",
            Duration::from_secs(5),
        )
        .unwrap();

        let error = copy_with(
            &["sh", "-c", "cat >/dev/null; exit 1"],
            "secret",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error, "sh failed (exit 1)");

        let error = copy_with(&["sleep", "5"], "secret", Duration::from_millis(200)).unwrap_err();
        assert_eq!(error, "sleep timed out");
    }

    #[test]
    fn test_copy_file_to_clipboard_preserves_binary_data() {
        let dir = std::env::temp_dir().join(format!("raffi-image-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input_path = dir.join("image.png");
        let output_path = dir.join("clipboard");
        let image = b"\x89PNG\r\n\x1a\n\x00\xff";
        std::fs::write(&input_path, image).unwrap();

        let mut command = Command::new("sh");
        command
            .args(["-c", "cat > \"$1\"", "copy-image"])
            .arg(&output_path);
        copy_file_to_clipboard_with(&mut command, &input_path, Duration::from_secs(5)).unwrap();

        assert_eq!(std::fs::read(output_path).unwrap(), image);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_terminal_tab_falls_back_only_before_asking_for_the_tab() {
        use crate::terminal::TabKind;
        let dir = std::env::temp_dir().join(format!("raffi-terminal-tab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("launches");
        let marker = dir.join("window");
        let window = [
            "sh".to_string(),
            "-c".to_string(),
            "touch \"$0\"".to_string(),
            marker.display().to_string(),
        ];
        // A fake kitty or WezTerm: its check answers with `check`, and a
        // launch is logged, then does `launch`.
        let fake = |kind: TabKind, check: &str, launch: &str| {
            let script = format!(
                "case \"$1\" in\n\
                 ls) {check} ;;\n\
                 list) echo '[{{\"window_id\": 3}}]' ;;\n\
                 list-clients) echo '[]' ;;\n\
                 *) echo \"$@\" >> \"$0\"; {launch} ;;\n\
                 esac"
            );
            let verb = match kind {
                TabKind::Kitty => "launch",
                TabKind::WezTerm => "spawn",
            };
            let tab = [
                "sh",
                "-c",
                &script,
                &log.display().to_string(),
                verb,
                "--",
                "x",
            ]
            .map(str::to_string);
            let _ = std::fs::remove_file(&log);
            let _ = std::fs::remove_file(&marker);
            let result =
                open_in_terminal_tab(kind, &tab, &window, None, Duration::from_millis(300));
            let launches = std::fs::read_to_string(&log).unwrap_or_default();
            (result, launches, marker.exists())
        };

        let (result, launches, window_opened) = fake(TabKind::Kitty, "exit 0", "exit 0");
        assert_eq!(
            (result, launches.as_str(), window_opened),
            (Ok(()), "launch -- x\n", false)
        );

        let (result, launches, window_opened) = fake(TabKind::Kitty, "exit 1", "exit 0");
        assert_eq!(
            (result, launches.as_str(), window_opened),
            (Ok(()), "", true),
            "remote control is off: a window opens"
        );

        // Once asked for, the tab may have opened whatever happens next:
        // a failed reply, a signal or no reply in time.
        for kind in [TabKind::Kitty, TabKind::WezTerm] {
            for launch in ["exit 1", "kill -9 $$", "sleep 5"] {
                let (result, launches, window_opened) = fake(kind, "exit 0", launch);
                let error = result.unwrap_err();
                assert!(error.contains("cannot tell whether"), "{launch}: {error}");
                assert_eq!(launches.lines().count(), 1, "{launch}: launched once");
                assert!(!window_opened, "{launch}: no window");
            }
        }

        let (result, launches, _) = fake(TabKind::WezTerm, "exit 0", "exit 0");
        assert_eq!(result, Ok(()));
        assert_eq!(
            launches, "spawn --window-id 3 -- x\n",
            "the window is chosen first"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_run_deferred_runs_once_and_reports_failures() {
        let slot = Mutex::new(Some(DeferredAction::Command(vec!["true".into()])));
        let mut reported = Vec::new();
        run_deferred_with(&slot, DeferredAction::run, |_, error| {
            reported.push(error.to_string())
        })
        .unwrap();
        assert!(slot.lock().unwrap().is_none());

        let mut runs = 0;
        run_deferred_with(
            &slot,
            |_| {
                runs += 1;
                Ok(())
            },
            |_, error| reported.push(error.to_string()),
        )
        .unwrap();
        assert_eq!(runs, 0);

        *slot.lock().unwrap() = Some(DeferredAction::Command(vec!["false".into()]));
        let error = run_deferred_with(&slot, DeferredAction::run, |_, error| {
            reported.push(error.to_string())
        })
        .unwrap_err();
        assert_eq!(error, "action: false failed (exit 1)");
        assert_eq!(reported, vec!["false failed (exit 1)".to_string()]);
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn test_run_batch_stops_at_first_failure_or_timeout() {
        let argv = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        run_batch(&[argv(&["true"]), argv(&["true"])], DEFERRED_TIMEOUT).unwrap();
        assert_eq!(
            run_batch(
                &[argv(&["false"]), argv(&["raffi-no-such-program"])],
                DEFERRED_TIMEOUT
            )
            .unwrap_err(),
            "false failed (exit 1)"
        );
        let started = Instant::now();
        assert_eq!(
            run_batch(
                &[argv(&["sleep", "5"]), argv(&["raffi-no-such-program"])],
                Duration::from_millis(100)
            )
            .unwrap_err(),
            "sleep: timed out after 0.1s"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_run_detached_leaves_long_running_commands_alone() {
        let started = Instant::now();
        run_detached(&["sleep".into(), "1".into()], Duration::from_millis(100)).unwrap();
        assert!(started.elapsed() < Duration::from_millis(900));
        assert!(
            run_detached(&["raffi-no-such-program".into()], DEFERRED_TIMEOUT)
                .unwrap_err()
                .starts_with("cannot start")
        );
    }

    #[test]
    fn test_describe_non_text_clipboard() {
        assert_eq!(describe_non_text("").as_deref(), Some("clipboard is empty"));
        assert_eq!(
            describe_non_text("image/png\nimage/jpeg\n").as_deref(),
            Some("clipboard has no text (it holds image/png, image/jpeg)")
        );
        assert_eq!(describe_non_text("image/png\ntext/plain\n"), None);
        assert_eq!(describe_non_text("UTF8_STRING\n"), None);
    }
}
