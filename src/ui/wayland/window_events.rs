//! Live updates for the window switcher: follow the compositor's event
//! stream and report when windows or workspaces change.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use iced::futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};

use super::windows::Compositor;

/// Changes closer together than this make one reload.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// How often an idle watcher checks that the switcher is still open.
const IDLE_CHECK: Duration = Duration::from_secs(1);

/// Hyprland events that change a window or workspace row. Workspace
/// switch events are included because the workspace rows' Current and
/// Visible badges and their move targets are focus-derived state, and Raffi
/// does not grab the keyboard, so switches happen while it is open. Window
/// focus events stay out: Raffi takes focus itself, and window rows do not
/// show it.
const HYPRLAND_EVENTS: [&str; 14] = [
    "openwindow",
    "closewindow",
    "movewindowv2",
    "windowtitlev2",
    "changefloatingmode",
    "pin",
    "fullscreen",
    "createworkspacev2",
    "destroyworkspacev2",
    "renameworkspace",
    "moveworkspacev2",
    "workspace",
    "workspacev2",
    "activespecial",
];

/// niri events that change a row. `WorkspaceActivated` covers workspace
/// switches, which the workspace rows' Current and Visible badges and their
/// move targets follow while the switcher is open. `WindowsChanged` and the
/// first `WorkspacesChanged` only describe the state when the stream starts.
const NIRI_EVENTS: [&str; 5] = [
    "WindowOpenedOrChanged",
    "WindowClosed",
    "WindowUrgencyChanged",
    "WorkspacesChanged",
    "WorkspaceActivated",
];

/// sway `change` values of window and workspace events that change a row.
/// `focus` is included for workspace switches, which the workspace rows'
/// badges and move targets follow; it also fires on plain window focus
/// changes, where nothing a row shows changes, and that extra debounced
/// reload is accepted churn.
const SWAY_CHANGES: [&str; 11] = [
    "new",
    "close",
    "title",
    "floating",
    "fullscreen_mode",
    "move",
    "urgent",
    "init",
    "empty",
    "rename",
    "focus",
];

/// Report each burst of window and workspace changes while the receiver is
/// kept; the watcher stops within [`IDLE_CHECK`] once it is dropped.
pub(super) fn watch(compositor: &Compositor) -> UnboundedReceiver<()> {
    let (sender, receiver) = unbounded();
    let compositor = *compositor;
    std::thread::spawn(move || {
        if let Err(error) = run(compositor, &sender) {
            crate::debug_log!("windows: no live updates from {compositor:?}: {error}");
        }
    });
    receiver
}

/// The open event source; closing it stops the reader thread.
enum Source {
    Socket(UnixStream),
    Child(Child),
}

impl Source {
    fn open(compositor: Compositor) -> Result<(Self, Box<dyn Read + Send>), String> {
        let spawn = |program: &str, args: &[&str]| {
            let mut child = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("{program}: {error}"))?;
            let stdout = child.stdout.take().ok_or("no event output")?;
            Ok((Self::Child(child), Box::new(stdout) as Box<dyn Read + Send>))
        };
        match compositor {
            Compositor::Hyprland => {
                let socket = hyprland_socket()?;
                let stream = UnixStream::connect(&socket)
                    .map_err(|error| format!("{}: {error}", socket.display()))?;
                let reader = stream.try_clone().map_err(|error| error.to_string())?;
                Ok((Self::Socket(stream), Box::new(reader)))
            }
            Compositor::Niri => spawn("niri", &["msg", "--json", "event-stream"]),
            Compositor::Sway => spawn(
                "swaymsg",
                &["-r", "-t", "subscribe", "-m", r#"["window","workspace"]"#],
            ),
        }
    }

    fn close(self) {
        match self {
            Self::Socket(stream) => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Self::Child(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn hyprland_socket() -> Result<PathBuf, String> {
    let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .filter(|instance| super::hyprland::valid_instance_signature(instance))
        .ok_or("no valid HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("no XDG_RUNTIME_DIR")?;
    Ok(PathBuf::from(runtime)
        .join("hypr")
        .join(instance)
        .join(".socket2.sock"))
}

fn run(compositor: Compositor, sender: &UnboundedSender<()>) -> Result<(), String> {
    let (source, reader) = Source::open(compositor)?;
    let (changed, changes) = mpsc::channel();
    std::thread::spawn(move || read_events(compositor, reader, |()| changed.send(()).is_ok()));
    loop {
        match changes.recv_timeout(IDLE_CHECK) {
            Ok(()) => {
                while changes.recv_timeout(DEBOUNCE).is_ok() {}
                if sender.unbounded_send(()).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) if sender.is_closed() => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    source.close();
    Ok(())
}

/// Call `changed` for every event of `reader` that changes a row, until it
/// returns false or the stream ends.
fn read_events(compositor: Compositor, reader: impl Read, mut changed: impl FnMut(()) -> bool) {
    let reader = BufReader::new(reader);
    match compositor {
        Compositor::Hyprland => {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if hyprland_changes_rows(&line) && !changed(()) {
                    break;
                }
            }
        }
        Compositor::Niri | Compositor::Sway => {
            let mut workspaces_seen = false;
            let events = serde_json::Deserializer::from_reader(reader).into_iter();
            for event in events {
                let Ok(event) = event else { break };
                let relevant = match compositor {
                    Compositor::Niri => niri_changes_rows(&event, &mut workspaces_seen),
                    _ => sway_changes_rows(&event),
                };
                if relevant && !changed(()) {
                    break;
                }
            }
        }
    }
}

fn hyprland_changes_rows(line: &str) -> bool {
    line.split_once(">>")
        .is_some_and(|(event, _)| HYPRLAND_EVENTS.contains(&event))
}

fn niri_changes_rows(event: &serde_json::Value, workspaces_seen: &mut bool) -> bool {
    let Some(name) = event
        .as_object()
        .and_then(|event| event.keys().next())
        .map(String::as_str)
    else {
        return false;
    };
    if name == "WorkspacesChanged" && !std::mem::replace(workspaces_seen, true) {
        return false;
    }
    NIRI_EVENTS.contains(&name)
}

fn sway_changes_rows(event: &serde_json::Value) -> bool {
    event
        .get("change")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|change| SWAY_CHANGES.contains(&change))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(compositor: Compositor, stream: &str) -> usize {
        let mut seen = 0;
        read_events(compositor, stream.as_bytes(), |()| {
            seen += 1;
            true
        });
        seen
    }

    #[test]
    fn test_hyprland_events_skip_window_focus() {
        let stream = "activewindow>>kitty,notes\n\
                      activewindowv2>>0x1a\n\
                      openwindow>>0x1a,2,kitty,notes\n\
                      movewindow>>0x1a,3\n\
                      movewindowv2>>0x1a,3,3\n\
                      workspace>>3,3\n\
                      workspacev2>>3,3\n\
                      activespecial>>special,3\n\
                      windowtitlev2>>0x1a,vim\n\
                      createworkspacev2>>4,4\n";
        assert_eq!(count(Compositor::Hyprland, stream), 7);
    }

    #[test]
    fn test_niri_events_skip_initial_state_and_window_focus() {
        let stream = r#"{"WorkspacesChanged":{"workspaces":[]}}
            {"WindowsChanged":{"windows":[]}}
            {"KeyboardLayoutsChanged":{}}
            {"WindowFocusChanged":{"id":2}}
            {"WindowOpenedOrChanged":{"window":{"id":3}}}
            {"WorkspaceActivated":{"id":1,"focused":true}}
            {"WorkspacesChanged":{"workspaces":[]}}
            {"WindowClosed":{"id":3}}"#;
        assert_eq!(count(Compositor::Niri, stream), 4);
    }

    #[test]
    fn test_sway_events_read_pretty_output() {
        let stream = r#"{
              "success": true
            }
            {
              "change": "focus",
              "container": {"id": 7}
            }
            {
              "change": "new",
              "container": {"id": 8}
            }
            {"change": "focus", "current": {"name": "2"}}
            {"change": "rename", "current": {"name": "web"}}"#;
        assert_eq!(count(Compositor::Sway, stream), 4);
    }

    #[test]
    fn test_read_events_stops_when_told() {
        let mut seen = 0;
        read_events(
            Compositor::Hyprland,
            "openwindow>>a\nclosewindow>>a\n".as_bytes(),
            |()| {
                seen += 1;
                false
            },
        );
        assert_eq!(seen, 1);
    }
}
