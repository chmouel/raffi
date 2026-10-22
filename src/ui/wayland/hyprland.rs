use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::actions::{kill_process_group, run_bounded, run_bounded_status, validate_snippet};
use super::placeholders::MAX_VALUE;
use crate::compositor::{lua_quote, lua_unavailable};

const TIMEOUT: Duration = Duration::from_secs(2);
const RESTORE_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct ClipboardRestore {
    previous: String,
    marker: String,
}

impl std::fmt::Debug for ClipboardRestore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardRestore")
            .field("marker", &self.marker)
            .finish_non_exhaustive()
    }
}

impl ClipboardRestore {
    fn snapshot(command: Command) -> Result<Self, String> {
        let bytes = run_bounded(command, TIMEOUT, MAX_VALUE)?;
        let previous = String::from_utf8(bytes).map_err(|_| "clipboard is not UTF-8 text")?;
        validate_snippet(&previous)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "cannot generate clipboard marker")?
            .as_nanos();
        Ok(Self {
            previous,
            marker: format!("text/x-raffi-{}-{nonce}", std::process::id()),
        })
    }

    fn restore(&self, types: Command, mut copy: Command) -> Result<(), String> {
        let types = run_bounded(types, TIMEOUT, 64 * 1024)?;
        if !String::from_utf8_lossy(&types)
            .lines()
            .any(|mime| mime == self.marker)
        {
            crate::debug_log!("hypr_paste: clipboard changed; skipping restoration");
            return Ok(());
        }
        copy.args(["--type", "text/plain;charset=utf-8", "--", &self.previous]);
        wait_status(copy, TIMEOUT)?;
        crate::debug_log!("hypr_paste: restored previous clipboard text");
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(super) struct Hyprland {
    instance: String,
    socket_dir: PathBuf,
    address: String,
    program: PathBuf,
    mods: &'static str,
    restoration: Option<ClipboardRestore>,
}

/// Window classes of terminals that paste the clipboard with Ctrl+Shift+V.
const TERMINAL_CLASSES: &[&str] = &[
    "alacritty",
    "foot",
    "footclient",
    "kitty",
    "com.mitchellh.ghostty",
    "org.wezfurlong.wezterm",
    "org.gnome.terminal",
    "org.gnome.console",
    "org.gnome.ptyxis",
    "org.kde.konsole",
    "konsole",
    "com.gexperts.tilix",
    "terminator",
    "xfce4-terminal",
    "rio",
    "st",
    "st-256color",
];

/// Modifiers for the paste shortcut: terminals reserve Ctrl+V, so use Ctrl+Shift+V there.
/// `extra` (from `general.terminal_classes`) extends the built-in list.
fn paste_mods(class: &str, extra: &[String]) -> &'static str {
    let is_terminal = TERMINAL_CLASSES
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .any(|known| known.eq_ignore_ascii_case(class));
    if !class.is_empty() && is_terminal {
        "CTRL SHIFT"
    } else {
        "CTRL"
    }
}

/// Whether Raffi runs inside a Hyprland session.
pub(super) fn in_session() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|value| !value.is_empty())
}

/// Capture the paste target when a snippet source needs it. Inside Hyprland,
/// sources without an `action` default to `hypr_paste`, but only when a paste
/// target was found; otherwise they keep the regular `copy` default.
pub(super) fn default_to_hypr_paste(
    sources: &mut [crate::TextSnippetSourceConfig],
    in_session: bool,
    capture: impl FnOnce() -> Result<Hyprland, String>,
) -> Option<Result<Hyprland, String>> {
    let explicit = sources.iter().any(|source| {
        source.action.as_deref() == Some("hypr_paste")
            || source.secondary_action.as_deref() == Some("hypr_paste")
    });
    let defaulting = in_session && sources.iter().any(|source| source.action.is_none());
    let target = (explicit || defaulting).then(capture)?;
    if defaulting {
        match &target {
            Ok(_) => sources
                .iter_mut()
                .filter(|source| source.action.is_none())
                .for_each(|source| source.action = Some("hypr_paste".into())),
            Err(error) => {
                crate::debug_log!("hypr_paste: not used as default action: {error}")
            }
        }
    }
    (explicit || target.is_ok()).then_some(target)
}

/// A window manager action run through `hyprctl` once Raffi has closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HyprAction {
    Focus {
        address: String,
    },
    /// Move the window to `workspace` without following it, then focus it.
    MoveHere {
        address: String,
        workspace: String,
    },
    /// Ask the window to close, as its close button would.
    Close {
        address: String,
    },
    /// Switch to `workspace`, a selector such as `3` or `name:web`.
    Workspace {
        workspace: String,
    },
    /// Show or hide the special workspace `name`, without its `special:`
    /// prefix.
    ToggleSpecial {
        name: String,
    },
    /// Move the window to `workspace` and follow it there.
    MoveTo {
        address: String,
        workspace: String,
    },
    Exit,
}

impl HyprAction {
    /// The Lua run by `hyprctl eval` (Hyprland 0.55+ with a Lua config).
    /// Window actions assert first because acting on a missing window only
    /// warns.
    pub(super) fn lua(&self) -> String {
        let window = |address: &str| {
            format!(
                "local w = hl.get_window(\"address:{address}\"); assert(w, \"window is gone\"); "
            )
        };
        let focus = "hl.dispatch(hl.dsp.focus({ window = w }))";
        match self {
            Self::Focus { address } => format!("{}{focus}", window(address)),
            Self::MoveHere { address, workspace } => format!(
                "{}hl.dispatch(hl.dsp.window.move({{ window = w, workspace = {}, follow = false }})); {focus}",
                window(address),
                lua_quote(workspace)
            ),
            Self::Close { address } => format!(
                "{}hl.dispatch(hl.dsp.window.close({{ window = w }}))",
                window(address)
            ),
            Self::Workspace { workspace } => format!(
                "hl.dispatch(hl.dsp.focus({{ workspace = {} }}))",
                lua_quote(workspace)
            ),
            Self::ToggleSpecial { name } => format!(
                "hl.dispatch(hl.dsp.workspace.toggle_special({}))",
                lua_quote(name)
            ),
            Self::MoveTo { address, workspace } => format!(
                "{}hl.dispatch(hl.dsp.window.move({{ window = w, workspace = {}, follow = true }}))",
                window(address),
                lua_quote(workspace)
            ),
            Self::Exit => "hl.dispatch(hl.dsp.exit())".into(),
        }
    }

    /// The `hyprctl dispatch` arguments for hyprlang configs, run in order.
    pub(super) fn legacy(&self) -> Vec<Vec<String>> {
        let focus = |address: &str| vec!["focuswindow".into(), format!("address:{address}")];
        match self {
            Self::Focus { address } => vec![focus(address)],
            Self::MoveHere { address, workspace } => vec![
                vec![
                    "movetoworkspacesilent".into(),
                    format!("{workspace},address:{address}"),
                ],
                focus(address),
            ],
            Self::Close { address } => {
                vec![vec!["closewindow".into(), format!("address:{address}")]]
            }
            Self::Workspace { workspace } => vec![vec!["workspace".into(), workspace.clone()]],
            Self::ToggleSpecial { name } => {
                vec![vec!["togglespecialworkspace".into(), name.clone()]]
            }
            Self::MoveTo { address, workspace } => vec![vec![
                "movetoworkspace".into(),
                format!("{workspace},address:{address}"),
            ]],
            Self::Exit => vec![vec!["exit".into()]],
        }
    }

    /// Run the action in the current Hyprland session.
    pub(super) fn run(&self) -> Result<(), String> {
        self.run_with(|args| {
            let mut command = Command::new("hyprctl");
            command.args(args);
            let (status, output) = run_bounded_status(command, TIMEOUT, 64 * 1024)
                .map_err(|error| format!("hyprctl: {error}"))?;
            Ok((
                status.success(),
                String::from_utf8_lossy(&output).into_owned(),
            ))
        })
    }

    /// Try the Lua form, falling back to `hyprctl dispatch` only when
    /// Hyprland has no Lua support. `hyprctl` returns success and the reply.
    fn run_with(
        &self,
        hyprctl: impl Fn(&[String]) -> Result<(bool, String), String>,
    ) -> Result<(), String> {
        let (success, reply) = hyprctl(&["eval".into(), self.lua()])?;
        if !lua_unavailable(&reply) {
            return checked_status(success, &reply);
        }
        crate::debug_log!(
            "hyprctl eval unavailable ({}); using dispatch",
            reply.trim()
        );
        self.legacy().into_iter().try_for_each(|args| {
            let argv: Vec<String> = std::iter::once("dispatch".to_string())
                .chain(args)
                .collect();
            let (success, reply) = hyprctl(&argv)?;
            checked_status(success, &reply)
        })
    }
}

fn checked_status(success: bool, reply: &str) -> Result<(), String> {
    match checked_reply_for(reply.as_bytes(), "action") {
        Ok(()) if !success => Err("hyprctl failed".into()),
        result => result,
    }
}

/// Show `message` as a Hyprland notification; outside Hyprland it is only
/// logged.
pub(super) fn notify(message: &str) {
    crate::debug_log!("notify: {message}");
    let command = if in_session() {
        let mut command = Command::new("hyprctl");
        command.args(["notify", "0", "5000", "rgb(ff5555)", message]);
        command
    } else {
        let mut command = Command::new("notify-send");
        command.args(["--app-name=Raffi", "--urgency=critical", "Raffi", message]);
        command
    };
    let _ = run_bounded(command, TIMEOUT, 64 * 1024);
}

pub(super) fn valid_instance_signature(instance: &str) -> bool {
    !instance.is_empty()
        && instance
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn hyprctl_command(program: &Path, instance: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.arg("-i").arg(instance).args(args);
    command
}

impl Hyprland {
    pub(super) fn capture(terminal_classes: &[String]) -> Result<Self, String> {
        let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| "hypr_paste requires a Hyprland session".to_string())?;
        if !valid_instance_signature(&instance) {
            return Err("invalid Hyprland instance signature".into());
        }
        let runtime =
            std::env::var_os("XDG_RUNTIME_DIR").ok_or("hypr_paste requires XDG_RUNTIME_DIR")?;
        let mut target = Self {
            socket_dir: PathBuf::from(runtime).join("hypr").join(&instance),
            instance,
            address: String::new(),
            program: "hyprctl".into(),
            mods: "CTRL",
            restoration: None,
        };
        let window = target.active_window()?;
        let class = window
            .get("class")
            .and_then(|class| class.as_str())
            .unwrap_or_default();
        if class == "com.chmouel.raffi" {
            return Err("the paste target is another Raffi window".into());
        }
        target.mods = paste_mods(class, terminal_classes);
        crate::debug_log!("hypr_paste: target class={class:?} mods={}", target.mods);
        target.address = window
            .get("address")
            .and_then(|address| address.as_str())
            .filter(|address| valid_address(address))
            .ok_or("no active Hyprland window to paste into")?
            .to_string();
        target.events()?;
        // Hyprland before 0.55 has no Lua API, so pasting would always fail.
        target
            .eval("assert(true)")
            .map_err(|error| format!("hypr_paste needs Hyprland 0.55 or later: {error}"))?;
        Ok(target)
    }

    fn command(&self, args: &[&str]) -> Command {
        hyprctl_command(&self.program, &self.instance, args)
    }

    fn output(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        run_bounded(self.command(args), TIMEOUT, 64 * 1024)
            .map_err(|error| format!("hyprctl: {error}"))
    }

    fn eval(&self, expression: &str) -> Result<(), String> {
        checked_reply(&self.output(&["eval", expression])?)
    }

    fn active_window(&self) -> Result<serde_json::Value, String> {
        serde_json::from_slice(&self.output(&["-j", "activewindow"])?)
            .map_err(|_| "hyprctl returned invalid active-window data".into())
    }

    fn events(&self) -> Result<UnixStream, String> {
        UnixStream::connect(self.socket_dir.join(".socket2.sock"))
            .map_err(|error| format!("cannot subscribe to Hyprland focus events: {error}"))
    }

    /// Fail when the captured paste target window has closed.
    pub(super) fn check_target(&self) -> Result<(), String> {
        self.eval(&format!(
            "assert(hl.get_window(\"address:{}\") ~= nil, \"paste target no longer exists\")",
            self.address
        ))
    }

    pub(super) fn prepare(&mut self, value: &str, restore_clipboard: bool) -> Result<(), String> {
        validate_snippet(value)?;
        self.check_target()?;
        self.restoration = if restore_clipboard {
            let mut command = Command::new("wl-paste");
            command.args(["--no-newline", "--type", "text"]);
            match ClipboardRestore::snapshot(command) {
                Ok(snapshot) => Some(snapshot),
                Err(error) => {
                    crate::debug_log!(
                        "hypr_paste: clipboard snapshot unavailable; skipping restoration: {error}"
                    );
                    None
                }
            }
        } else {
            None
        };
        self.publish(|| Command::new("wl-copy"), value)
    }

    fn publish(&self, wl_copy: impl Fn() -> Command, value: &str) -> Result<(), String> {
        // wl-copy advertises standard plain-text formats alongside text/* types.
        let mime = self
            .restoration
            .as_ref()
            .map_or("text/plain;charset=utf-8", |r| r.marker.as_str());
        let copy = |flags: &[&str]| {
            let mut command = wl_copy();
            command.args(flags).args(["--type", mime, "--", value]);
            wait_status(command, TIMEOUT)
        };
        // wl-clipboard before 2.3 rejects --sensitive; copy without the hint there.
        copy(&["--sensitive"])
            .or_else(|error| {
                crate::debug_log!("hypr_paste: sensitive copy failed ({error}); retrying without");
                copy(&[])
            })
            .map_err(|error| format!("wl-copy (wl-clipboard): {error}"))
    }

    pub(super) fn paste(&self) -> Result<(), String> {
        // Subscribe before querying focus so returning focus cannot be missed.
        let events = self.events()?;
        let deadline = Instant::now() + TIMEOUT;
        let focused = self
            .active_window()?
            .get("address")
            .and_then(|v| v.as_str())
            .is_some_and(|address| address == self.address);
        wait_for_focus(events, &self.address, focused, deadline)?;
        // Check focus again inside the dispatch to reject stale/queued events.
        self.eval(&format!(
            "local w = hl.get_window(\"activewindow\"); assert(w and w.address == \"{}\", \"paste target lost focus\"); hl.dispatch(hl.dsp.send_shortcut({{ mods = \"{}\", key = \"v\", window = w }}))",
            self.address, self.mods
        ))?;
        if let Some(restoration) = &self.restoration {
            std::thread::sleep(RESTORE_DELAY);
            let mut types = Command::new("wl-paste");
            types.arg("--list-types");
            if let Err(error) = restoration.restore(types, Command::new("wl-copy")) {
                crate::debug_log!("hypr_paste: clipboard restoration skipped or failed: {error}");
            }
        }
        Ok(())
    }

    pub(super) fn notify_error(&self, error: &str) {
        crate::debug_log!("hypr_paste failed: {error}");
        let message = format!("Raffi could not paste: {error}");
        let _ = self.output(&["notify", "0", "5000", "rgb(ff5555)", &message]);
    }
}

#[cfg(test)]
impl Hyprland {
    pub(super) fn test_target() -> Self {
        Self {
            instance: "test".into(),
            socket_dir: PathBuf::new(),
            address: "0x123".into(),
            program: "hyprctl".into(),
            mods: "CTRL",
            restoration: None,
        }
    }
}

pub(super) fn valid_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|digits| {
        !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_hexdigit())
            && digits.bytes().any(|b| b != b'0')
    })
}

fn checked_reply(output: &[u8]) -> Result<(), String> {
    checked_reply_for(output, "paste operation")
}

fn checked_reply_for(output: &[u8], operation: &str) -> Result<(), String> {
    let reply = String::from_utf8_lossy(output);
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(format!(
            "Hyprland rejected the {operation}: {}",
            reply.trim()
        ))
    }
}

fn wait_for_focus(
    events: UnixStream,
    address: &str,
    focused: bool,
    deadline: Instant,
) -> Result<(), String> {
    if focused {
        return Ok(());
    }
    let mut reader = BufReader::new(events);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for the original window to regain focus".into());
        }
        reader
            .get_ref()
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("cannot watch Hyprland focus: {error}"))?;
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err("Hyprland disconnected before the paste target regained focus".into())
            }
            Ok(_) => {
                if let Some(event) = line.trim().strip_prefix("activewindowv2>>") {
                    if event.trim_start_matches("0x") == address.trim_start_matches("0x") {
                        return Ok(());
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err("timed out waiting for the original window to regain focus".into());
            }
            Err(error) => return Err(format!("cannot watch Hyprland focus: {error}")),
        }
    }
}

fn wait_status(mut command: Command, timeout: Duration) -> Result<(), String> {
    // wl-copy's serving child outlives its parent; do not wait for pipe EOF.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            outcome => {
                kill_process_group(child.id());
                let _ = child.wait();
                return Err(match outcome {
                    Err(error) => format!("cannot wait: {error}"),
                    _ => "timed out setting the clipboard".into(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script, "test"]);
        command
    }

    #[test]
    fn test_clipboard_snapshot_preserves_text_and_rejects_unusable_data() {
        let previous = "it's \"quoted\"\nhéllo\n$(touch injected)\n";
        let mut command = shell("printf '%s' \"$PREVIOUS\"");
        command.env("PREVIOUS", previous);
        let snapshot = ClipboardRestore::snapshot(command).unwrap();
        assert_eq!(snapshot.previous, previous);
        assert!(snapshot.marker.starts_with("text/x-raffi-"));
        assert!(!format!("{snapshot:?}").contains(previous));
        assert!(ClipboardRestore::snapshot(shell("exit 1")).is_err());
        assert!(ClipboardRestore::snapshot(shell("printf '\\377'")).is_err());
        assert!(ClipboardRestore::snapshot(shell("printf 'a\\000b'")).is_err());
        assert!(ClipboardRestore::snapshot(shell("head -c 102401 /dev/zero")).is_err());
    }

    #[test]
    fn test_snippet_publication_is_sensitive_when_wl_copy_supports_it() {
        let dir = std::env::temp_dir().join(format!("raffi-publish-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("args");
        let snapshot = ClipboardRestore {
            previous: "previous".into(),
            marker: "text/x-raffi-test".into(),
        };
        let failed_snapshot = ClipboardRestore::snapshot(shell("exit 1")).ok();
        let value = "--a snippet with 'quotes'\nand Unicode: café";
        for restoration in [None, Some(snapshot), failed_snapshot] {
            let mime = if restoration.is_some() {
                "text/x-raffi-test"
            } else {
                "text/plain;charset=utf-8"
            };
            let target = Hyprland {
                restoration,
                ..Hyprland::test_target()
            };
            // wl-clipboard before 2.3 rejects --sensitive.
            for old_wl_copy in [false, true] {
                let wl_copy = || {
                    let mut command = shell(
                        r#"
if [ "$1" = --sensitive ] && [ -n "$OLD" ]; then exit 1; fi
printf '%s|' "$@" > "$OUT"
"#,
                    );
                    command.env("OUT", &out);
                    if old_wl_copy {
                        command.env("OLD", "1");
                    }
                    command
                };
                target.publish(wl_copy, value).unwrap();
                let sensitive = if old_wl_copy { "" } else { "--sensitive|" };
                assert_eq!(
                    std::fs::read_to_string(&out).unwrap(),
                    format!("{sensitive}--type|{mime}|--|{value}|")
                );
            }
        }
        let target = Hyprland::test_target();
        assert!(target.publish(|| shell("exit 1"), value).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_restore_requires_our_marker_and_preserves_new_copies() {
        let dir = std::env::temp_dir().join(format!("raffi-restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("restored");
        let snapshot = ClipboardRestore {
            previous: "previous\ntext\n".into(),
            marker: "text/x-raffi-test".into(),
        };
        for (types, expected) in [
            ("text/plain\ntext/x-raffi-test\n", true),
            ("text/plain\n", false),
            ("text/plain\ntext/x-raffi-another-copy\n", false),
            ("image/png\n", false),
        ] {
            let mut offered = shell("printf '%s' \"$TYPES\"");
            offered.env("TYPES", types);
            let mut copy = shell("printf '%s' \"$4\" > \"$OUT\"");
            copy.env("OUT", &out);
            snapshot.restore(offered, copy).unwrap();
            assert_eq!(out.exists(), expected);
            if expected {
                assert_eq!(std::fs::read_to_string(&out).unwrap(), snapshot.previous);
                std::fs::remove_file(&out).unwrap();
            }
        }
        assert!(snapshot.restore(shell("exit 1"), shell("exit 9")).is_err());
        assert!(snapshot
            .restore(shell("echo text/x-raffi-test"), shell("exit 9"))
            .is_err());
        assert!(!out.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_paste_shortcut_uses_ctrl_shift_v_for_terminals() {
        for class in [
            "kitty",
            "foot",
            "Alacritty",
            "com.mitchellh.ghostty",
            "org.wezfurlong.wezterm",
        ] {
            assert_eq!(paste_mods(class, &[]), "CTRL SHIFT", "{class}");
        }
        for class in ["", "firefox", "code", "org.gnome.TextEditor", "kittyish"] {
            assert_eq!(paste_mods(class, &[]), "CTRL", "{class}");
        }
        let extra = vec!["My.Term".to_string(), String::new()];
        assert_eq!(paste_mods("my.term", &extra), "CTRL SHIFT");
        assert_eq!(paste_mods("kitty", &extra), "CTRL SHIFT");
        assert_eq!(paste_mods("", &extra), "CTRL");
        assert_eq!(paste_mods("firefox", &extra), "CTRL");
    }

    #[test]
    fn test_target_address_rejects_missing_and_unsafe_windows() {
        for address in ["", "0x", "0x0", "0x000", "123", "0x123\";error('bad')"] {
            assert!(!valid_address(address), "{address}");
        }
        assert!(valid_address("0xABC123"));
    }

    #[test]
    fn test_focus_wait_handles_current_delayed_and_missing_focus() {
        let (events, _writer) = UnixStream::pair().unwrap();
        wait_for_focus(events, "0x123", true, Instant::now()).unwrap();

        let (events, mut writer) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            writer.write_all(b"activewindowv2>>999\n").unwrap();
            std::thread::sleep(Duration::from_millis(20));
            writer.write_all(b"activewindowv2>>123\n").unwrap();
        });
        wait_for_focus(events, "0x123", false, Instant::now() + TIMEOUT).unwrap();
        sender.join().unwrap();

        let (events, _writer) = UnixStream::pair().unwrap();
        let error = wait_for_focus(
            events,
            "0x123",
            false,
            Instant::now() + Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        let (events, writer) = UnixStream::pair().unwrap();
        drop(writer);
        assert!(
            wait_for_focus(events, "0x123", false, Instant::now() + TIMEOUT)
                .unwrap_err()
                .contains("disconnected")
        );
    }

    #[test]
    fn test_clipboard_status_checks_failure_timeout_and_parent_exit() {
        let shell = |script: &str| {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        };
        let error = wait_status(shell("exit 42"), TIMEOUT).unwrap_err();
        assert!(error.contains("42"), "{error}");
        let error = wait_status(shell("sleep 3"), Duration::from_millis(30)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        let started = Instant::now();
        wait_status(shell("sleep 2 & exit 0"), TIMEOUT).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            wait_status(Command::new("raffi-no-such-copy-program"), TIMEOUT)
                .unwrap_err()
                .contains("cannot start")
        );
    }

    /// Another test thread forking while a script is being written makes running
    /// it fail with ETXTBSY until that child execs; wait for this to clear.
    fn wait_until_runnable(program: &std::path::Path) {
        for _ in 0..200 {
            match Command::new(program).output() {
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                _ => return,
            }
        }
    }

    #[test]
    fn test_hypr_action_renders_lua_and_legacy_forms() {
        let focus = HyprAction::Focus {
            address: "0x1a".into(),
        };
        assert_eq!(
            focus.lua(),
            r#"local w = hl.get_window("address:0x1a"); assert(w, "window is gone"); hl.dispatch(hl.dsp.focus({ window = w }))"#
        );
        assert_eq!(focus.legacy(), vec![vec!["focuswindow", "address:0x1a"]]);

        let move_here = HyprAction::MoveHere {
            address: "0x1a".into(),
            workspace: "special:magic".into(),
        };
        assert_eq!(
            move_here.lua(),
            r#"local w = hl.get_window("address:0x1a"); assert(w, "window is gone"); hl.dispatch(hl.dsp.window.move({ window = w, workspace = "special:magic", follow = false })); hl.dispatch(hl.dsp.focus({ window = w }))"#
        );
        assert_eq!(
            move_here.legacy(),
            vec![
                vec!["movetoworkspacesilent", "special:magic,address:0x1a"],
                vec!["focuswindow", "address:0x1a"],
            ]
        );

        let close = HyprAction::Close {
            address: "0x1a".into(),
        };
        assert_eq!(
            close.lua(),
            r#"local w = hl.get_window("address:0x1a"); assert(w, "window is gone"); hl.dispatch(hl.dsp.window.close({ window = w }))"#
        );
        assert_eq!(close.legacy(), vec![vec!["closewindow", "address:0x1a"]]);

        let workspace = HyprAction::Workspace {
            workspace: "name:web".into(),
        };
        assert_eq!(
            workspace.lua(),
            r#"hl.dispatch(hl.dsp.focus({ workspace = "name:web" }))"#
        );
        assert_eq!(workspace.legacy(), vec![vec!["workspace", "name:web"]]);

        let special = HyprAction::ToggleSpecial {
            name: "magic".into(),
        };
        assert_eq!(
            special.lua(),
            r#"hl.dispatch(hl.dsp.workspace.toggle_special("magic"))"#
        );
        assert_eq!(
            special.legacy(),
            vec![vec!["togglespecialworkspace", "magic"]]
        );

        let move_to = HyprAction::MoveTo {
            address: "0x1a".into(),
            workspace: "3".into(),
        };
        assert_eq!(
            move_to.lua(),
            r#"local w = hl.get_window("address:0x1a"); assert(w, "window is gone"); hl.dispatch(hl.dsp.window.move({ window = w, workspace = "3", follow = true }))"#
        );
        assert_eq!(
            move_to.legacy(),
            vec![vec!["movetoworkspace", "3,address:0x1a"]]
        );

        assert_eq!(HyprAction::Exit.lua(), "hl.dispatch(hl.dsp.exit())");
        assert_eq!(HyprAction::Exit.legacy(), vec![vec!["exit"]]);
    }

    #[test]
    fn test_lua_quote_escapes_hostile_names() {
        assert_eq!(lua_quote("name:web"), r#""name:web""#);
        assert_eq!(
            lua_quote(r#"name:a" }) os.exit() --\"#),
            r#""name:a\" }) os.exit() --\\""#
        );
        assert_eq!(
            lua_quote("a\nb\r\0\x1b\x7f9"),
            r#""a\010b\013\000\027\1279""#
        );
        assert_eq!(lua_quote("\u{e9}t\u{e9}"), "\"\u{e9}t\u{e9}\"");
    }

    #[test]
    fn test_hypr_action_falls_back_only_without_lua() {
        let move_here = HyprAction::MoveHere {
            address: "0x1a".into(),
            workspace: "2".into(),
        };
        let run = |eval_reply: &'static str, eval_ok: bool, dispatch_reply: &'static str| {
            let calls = std::cell::RefCell::new(Vec::new());
            let result = move_here.run_with(|args| {
                calls.borrow_mut().push(args.join(" "));
                Ok(if args[0] == "eval" {
                    (eval_ok, eval_reply.to_string())
                } else {
                    (true, dispatch_reply.to_string())
                })
            });
            (result, calls.into_inner())
        };

        let (result, calls) = run("ok\n", true, "");
        assert!(result.is_ok());
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("eval local w"));

        for reply in [
            "unknown request",
            "eval is only supported with the lua config manager",
        ] {
            let (result, calls) = run(reply, true, "ok");
            assert!(result.is_ok(), "{reply}");
            assert_eq!(
                calls[1..],
                [
                    "dispatch movetoworkspacesilent 2,address:0x1a",
                    "dispatch focuswindow address:0x1a",
                ]
            );
        }

        let (result, calls) = run("unknown request", true, "Invalid dispatcher");
        assert_eq!(
            result.unwrap_err(),
            "Hyprland rejected the action: Invalid dispatcher"
        );
        assert_eq!(calls.len(), 2, "stops at the first failed dispatch");

        let gone = "error: [string \"local w = ...\"]:1: window is gone";
        let (result, calls) = run(gone, false, "ok");
        assert_eq!(
            result.unwrap_err(),
            format!("Hyprland rejected the action: {gone}")
        );
        assert_eq!(calls.len(), 1, "Lua errors do not fall back");

        let (result, calls) = run("error: unknown request in lua", false, "ok");
        assert!(result.is_err());
        assert_eq!(calls.len(), 1);

        let (result, _) = run("ok", false, "ok");
        assert_eq!(result.unwrap_err(), "hyprctl failed");
    }

    /// Exercises the real deferred path on a throwaway window, which it
    /// closes:
    /// `RAFFI_HYPR_TEST_ADDRESS=0x.. RAFFI_HYPR_TEST_WORKSPACE=2 cargo test --all-features -- --ignored live_hypr`
    #[test]
    #[ignore]
    fn live_hypr_actions() {
        let address = std::env::var("RAFFI_HYPR_TEST_ADDRESS").unwrap();
        let workspace = std::env::var("RAFFI_HYPR_TEST_WORKSPACE").unwrap();
        let slot = |action| {
            std::sync::Mutex::new(Some(super::super::actions::DeferredAction::Hypr(action)))
        };
        super::super::actions::run_deferred(&slot(HyprAction::Focus {
            address: address.clone(),
        }))
        .unwrap();
        super::super::actions::run_deferred(&slot(HyprAction::MoveHere {
            address: address.clone(),
            workspace: workspace.clone(),
        }))
        .unwrap();
        // Follow the window to a special workspace, hide it, come back, and
        // close the window.
        for action in [
            HyprAction::MoveTo {
                address: address.clone(),
                workspace: "special:raffilive".into(),
            },
            HyprAction::ToggleSpecial {
                name: "raffilive".into(),
            },
            HyprAction::Workspace { workspace },
            HyprAction::Close { address },
        ] {
            super::super::actions::run_deferred(&slot(action)).unwrap();
        }
        assert!(
            super::super::actions::run_deferred(&slot(HyprAction::Focus {
                address: "0xdead".into(),
            }))
            .unwrap_err()
            .contains("window is gone")
        );
    }

    #[test]
    fn test_paste_dispatch_is_targeted_and_checks_reply() {
        let dir = std::env::temp_dir().join(format!("raffi-hypr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join(".socket2.sock")).unwrap();
        let program = dir.join("hyprctl");
        let called = dir.join("called");
        let script = format!(
            r#"#!/bin/sh
if [ "$3" = -j ]; then
    printf '%s' '{{"address":"0x123"}}'
else
    case "$4" in
        *'w.address == "0x123"'*'send_shortcut'*'mods = "CTRL SHIFT"'*'window = w'*) touch '{}' ;;
        *) exit 9 ;;
    esac
    printf 'ok\n'
fi
"#,
            called.display()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        wait_until_runnable(&program);
        let target = Hyprland {
            socket_dir: dir.clone(),
            program: program.clone(),
            mods: "CTRL SHIFT",
            ..Hyprland::test_target()
        };
        target.paste().unwrap();
        assert!(called.exists());

        std::fs::remove_file(&called).unwrap();
        std::fs::write(&program, "#!/bin/sh\nif [ \"$3\" = -j ]; then printf '%s' '{\"address\":\"0x999\"}'; else exit 9; fi\n").unwrap();
        wait_until_runnable(&program);
        let mut target = target.clone();
        // Drop all pending event connections, then close the next connection.
        while let Ok((stream, _)) = {
            listener.set_nonblocking(true).unwrap();
            listener.accept()
        } {
            drop(stream);
        }
        listener.set_nonblocking(false).unwrap();
        let server = std::thread::spawn(move || drop(listener.accept().unwrap()));
        assert!(target.paste().unwrap_err().contains("disconnected"));
        server.join().unwrap();
        assert!(!called.exists());

        assert!(checked_reply(b"error: paste target lost focus").is_err());
        assert!(checked_reply(b"").is_err());
        assert!(checked_reply(b"ok\n").is_ok());
        assert!(target.prepare("a\0b", false).unwrap_err().contains("NUL"));
        assert!(target
            .prepare(
                &"x".repeat(super::super::placeholders::MAX_VALUE + 1),
                false
            )
            .unwrap_err()
            .contains("larger than"));
        assert!(target
            .prepare("value", false)
            .unwrap_err()
            .contains("exited"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
