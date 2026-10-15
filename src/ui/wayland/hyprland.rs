use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::actions::{kill_process_group, run_bounded, validate_snippet};
use super::placeholders::MAX_VALUE;

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

impl Hyprland {
    pub(super) fn capture(terminal_classes: &[String]) -> Result<Self, String> {
        let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| "hypr_paste requires a Hyprland session".to_string())?;
        if instance.is_empty()
            || !instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
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
        Ok(target)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("-i").arg(&self.instance).args(args);
        command
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

    pub(super) fn prepare(&mut self, value: &str, restore_clipboard: bool) -> Result<(), String> {
        validate_snippet(value)?;
        self.eval(&format!(
            "assert(hl.get_window(\"address:{}\") ~= nil, \"paste target no longer exists\")",
            self.address
        ))?;
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
        self.publish(Command::new("wl-copy"), value)
    }

    fn publish(&self, mut command: Command, value: &str) -> Result<(), String> {
        // wl-copy advertises standard plain-text formats alongside text/* types.
        let mime = self
            .restoration
            .as_ref()
            .map_or("text/plain;charset=utf-8", |r| r.marker.as_str());
        command.args(["--sensitive", "--type", mime, "--", value]);
        wait_status(command, TIMEOUT).map_err(|error| format!("wl-copy (wl-clipboard): {error}"))
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
    fn test_snippet_publication_is_sensitive_with_or_without_restoration() {
        let snapshot = ClipboardRestore {
            previous: "previous".into(),
            marker: "text/x-raffi-test".into(),
        };
        let failed_snapshot = ClipboardRestore::snapshot(shell("exit 1")).ok();
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
            let value = "--a snippet with 'quotes'\nand Unicode: café";
            let mut command = shell(
                r#"
test "$#" = 5 &&
test "$1" = --sensitive &&
test "$2" = --type &&
test "$3" = "$EXPECTED_MIME" &&
test "$4" = -- &&
test "$5" = "$EXPECTED_VALUE"
"#,
            );
            command
                .env("EXPECTED_MIME", mime)
                .env("EXPECTED_VALUE", value);
            target.publish(command, value).unwrap();
        }
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

fn valid_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|digits| {
        !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_hexdigit())
            && digits.bytes().any(|b| b != b'0')
    })
}

fn checked_reply(output: &[u8]) -> Result<(), String> {
    let reply = String::from_utf8_lossy(output);
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(format!(
            "Hyprland rejected the paste operation: {}",
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
