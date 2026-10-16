use std::cmp::Reverse;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::actions::{kill_process_group, run_bounded, DeferredAction};
use super::command_list::{CommandAction, CommandItem};
use super::hyprland::{valid_address, HyprAction};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// The compositor Raffi runs in, for window switching and log out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Compositor {
    Hyprland,
    Niri,
}

impl Compositor {
    /// Detect the compositor from the environment. `var` reads a variable.
    pub fn detect(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let set = |name| var(name).is_some_and(|value| !value.is_empty());
        if set("HYPRLAND_INSTANCE_SIGNATURE") {
            Some(Self::Hyprland)
        } else if set("NIRI_SOCKET") {
            Some(Self::Niri)
        } else {
            None
        }
    }

    pub fn current() -> Option<Self> {
        Self::detect(|name| std::env::var(name).ok())
    }

    /// The program the window switcher talks to.
    pub fn program(self) -> &'static str {
        match self {
            Self::Hyprland => "hyprctl",
            Self::Niri => "niri",
        }
    }

    /// List the windows, most recently focused first. Blocking: run it off
    /// the UI thread.
    pub fn load_windows(self) -> Result<Vec<CommandItem>, String> {
        let program = self.program();
        let run = |args: &[&str]| {
            let mut command = Command::new(program);
            command.args(args);
            run_bounded(command, TIMEOUT, MAX_OUTPUT).map_err(|error| format!("{program}: {error}"))
        };
        let own_pid = std::process::id();
        match self {
            Self::Hyprland => hyprland_items(
                &run(&["clients", "-j"])?,
                &run(&["monitors", "-j"])?,
                own_pid,
            ),
            Self::Niri => niri_items(
                &run(&["msg", "--json", "windows"])?,
                &run(&["msg", "--json", "workspaces"])?,
                own_pid,
            ),
        }
    }
}

/// What a window row needs, whatever the compositor.
struct Window {
    identity: String,
    title: String,
    app: String,
    app_label: &'static str,
    workspace: String,
    icon_names: Vec<String>,
    focus: DeferredAction,
    move_here: Option<DeferredAction>,
}

fn window_item(window: Window) -> CommandItem {
    let title = if window.title.is_empty() {
        window.app.clone()
    } else {
        window.title
    };
    let subtitle = match (window.app.is_empty(), window.workspace.is_empty()) {
        (_, true) => window.app.clone(),
        (true, false) => format!("Workspace {}", window.workspace),
        (false, false) => format!("{} on workspace {}", window.app, window.workspace),
    };
    let mut details = vec![
        (window.app_label.to_string(), window.app.clone()),
        ("Workspace".to_string(), window.workspace),
        ("Enter".to_string(), "Focus the window".to_string()),
    ];
    let action = |action| CommandAction {
        action,
        confirm: None,
        keep_open: false,
        progress: None,
    };
    if window.move_here.is_some() {
        details.push((
            "Ctrl+Enter".to_string(),
            "Move it to the current workspace and focus it".to_string(),
        ));
    }
    CommandItem {
        match_text: format!("{title} {}", window.app),
        title,
        subtitle,
        icon_names: window.icon_names,
        icon: None,
        identity: window.identity,
        unavailable: None,
        primary: Some(action(window.focus)),
        secondary: window.move_here.map(action),
        details,
    }
}

/// Icon names to try for `names`, as given and lowercased, without repeats.
fn icon_names(names: &[&str]) -> Vec<String> {
    let mut icons: Vec<String> = Vec::new();
    for name in names {
        for name in [name.to_string(), name.to_lowercase()] {
            if !name.is_empty() && !icons.contains(&name) {
                icons.push(name);
            }
        }
    }
    icons
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Client {
    address: String,
    #[serde(default = "mapped_default")]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    workspace: Workspace,
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    initial_class: String,
    pid: i64,
    #[serde(rename = "focusHistoryID", default)]
    focus_history_id: i64,
    #[serde(default)]
    grouped: Vec<String>,
}

fn mapped_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Workspace {
    id: i64,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Monitor {
    #[serde(default)]
    focused: bool,
    active_workspace: Workspace,
    special_workspace: Option<Workspace>,
}

/// Build the window rows from `hyprctl clients -j` and `hyprctl monitors -j`
/// output, leaving out the windows of `own_pid`.
pub(super) fn hyprland_items(
    clients: &[u8],
    monitors: &[u8],
    own_pid: u32,
) -> Result<Vec<CommandItem>, String> {
    let mut clients: Vec<Client> = serde_json::from_slice(clients)
        .map_err(|_| "hyprctl returned invalid window data".to_string())?;
    let monitors: Vec<Monitor> = serde_json::from_slice(monitors)
        .map_err(|_| "hyprctl returned invalid workspace data".to_string())?;
    let target = monitors
        .iter()
        .find(|monitor| monitor.focused)
        .or(monitors.first())
        .and_then(|monitor| {
            // A special workspace shown over the regular one is where the
            // user is looking.
            match &monitor.special_workspace {
                Some(special) if !special.name.is_empty() => move_target(special),
                _ => move_target(&monitor.active_workspace),
            }
        });

    clients.retain(|client| {
        // Inactive tabs of a group are hidden but still reachable.
        client.mapped
            && (!client.hidden || !client.grouped.is_empty())
            && client.pid != i64::from(own_pid)
            && !(client.title.is_empty() && client.class.is_empty())
            && valid_address(&client.address)
    });
    // Windows never focused report -1; they go last.
    clients.sort_by_key(|client| (client.focus_history_id < 0, client.focus_history_id));
    Ok(clients
        .into_iter()
        .map(|client| hyprland_window(client, target.as_deref()))
        .collect())
}

/// The workspace selector for moving a window to the active workspace. Named
/// workspaces have negative ids, which Hyprland would read as relative moves,
/// and the legacy dispatch cannot take names with commas.
fn move_target(active: &Workspace) -> Option<String> {
    if active.id > 0 {
        Some(active.id.to_string())
    } else if active.name.contains(',') {
        None
    } else if active.name.starts_with("special:") {
        Some(active.name.clone())
    } else if !active.name.is_empty() {
        Some(format!("name:{}", active.name))
    } else {
        None
    }
}

fn hyprland_window(client: Client, target: Option<&str>) -> CommandItem {
    let workspace = if client.workspace.name.is_empty() {
        client.workspace.id.to_string()
    } else {
        client.workspace.name.clone()
    };
    window_item(Window {
        identity: client.address.clone(),
        title: client.title,
        icon_names: icon_names(&[&client.class, &client.initial_class]),
        app: client.class,
        app_label: "Class",
        workspace,
        move_here: target.map(|target| {
            DeferredAction::Hypr(HyprAction::MoveHere {
                address: client.address.clone(),
                workspace: target.to_string(),
            })
        }),
        focus: DeferredAction::Hypr(HyprAction::Focus {
            address: client.address,
        }),
    })
}

#[derive(Debug, Deserialize)]
struct NiriWindow {
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    workspace_id: Option<u64>,
    #[serde(default)]
    is_focused: bool,
    /// Missing in older niri versions.
    #[serde(default)]
    focus_timestamp: Option<NiriTimestamp>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
struct NiriTimestamp {
    secs: u64,
    nanos: u32,
}

#[derive(Debug, Deserialize)]
struct NiriWorkspace {
    id: u64,
    idx: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    is_focused: bool,
}

/// Build the window rows from `niri msg --json windows` and `niri msg --json
/// workspaces` output, leaving out the windows of `own_pid`.
pub(super) fn niri_items(
    windows: &[u8],
    workspaces: &[u8],
    own_pid: u32,
) -> Result<Vec<CommandItem>, String> {
    let mut windows: Vec<NiriWindow> = serde_json::from_slice(windows)
        .map_err(|_| "niri returned invalid window data".to_string())?;
    let workspaces: Vec<NiriWorkspace> = serde_json::from_slice(workspaces)
        .map_err(|_| "niri returned invalid workspace data".to_string())?;
    let focused = workspaces.iter().find(|workspace| workspace.is_focused);
    let several_outputs = workspaces
        .iter()
        .any(|workspace| workspace.output != workspaces[0].output);

    windows.retain(|window| {
        window.pid != Some(i64::from(own_pid))
            && !(window.title.as_deref().unwrap_or_default().is_empty()
                && window.app_id.as_deref().unwrap_or_default().is_empty())
    });
    // Older niri has no focus timestamps: the focused window first, then the
    // niri order.
    windows.sort_by_key(|window| Reverse((window.is_focused, window.focus_timestamp)));
    Ok(windows
        .into_iter()
        .map(|window| {
            let workspace = workspaces
                .iter()
                .find(|workspace| Some(workspace.id) == window.workspace_id);
            niri_window(window, workspace, focused, several_outputs)
        })
        .collect())
}

fn niri_window(
    window: NiriWindow,
    workspace: Option<&NiriWorkspace>,
    focused: Option<&NiriWorkspace>,
    several_outputs: bool,
) -> CommandItem {
    let mut label = workspace.map_or_else(String::new, |workspace| {
        workspace
            .name
            .clone()
            .unwrap_or_else(|| workspace.idx.to_string())
    });
    if let Some(output) = workspace
        .and_then(|workspace| workspace.output.as_deref())
        .filter(|_| several_outputs)
    {
        label = format!("{label} ({output})");
    }
    let id = window.id.to_string();
    let app = window.app_id.unwrap_or_default();
    window_item(Window {
        title: window.title.unwrap_or_default(),
        icon_names: icon_names(&[&app]),
        app,
        app_label: "App ID",
        workspace: label,
        move_here: focused.map(|focused| {
            DeferredAction::CommandBatch(niri_move_here(
                &id,
                workspace.and_then(|workspace| workspace.output.as_deref()),
                focused,
            ))
        }),
        focus: DeferredAction::Command(niri_action(&["focus-window", "--id", &id])),
        identity: id,
    })
}

fn niri_action(args: &[&str]) -> Vec<String> {
    ["niri", "msg", "action"]
        .iter()
        .chain(args)
        .map(|arg| arg.to_string())
        .collect()
}

/// The commands moving window `id`, now on `output`, to the `focused`
/// workspace and focusing it. A name is found on any monitor, but niri reads
/// a number as an index on the window's own monitor, so a window elsewhere
/// first moves to the focused monitor.
fn niri_move_here(id: &str, output: Option<&str>, focused: &NiriWorkspace) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let reference = match &focused.name {
        Some(name) if name.parse::<i32>().is_err() => name.clone(),
        _ => {
            if let Some(target) = focused
                .output
                .as_deref()
                .filter(|&target| Some(target) != output)
            {
                commands.push(niri_action(&["move-window-to-monitor", "--id", id, target]));
            }
            focused.idx.to_string()
        }
    };
    commands.push(niri_action(&[
        "move-window-to-workspace",
        "--window-id",
        id,
        "--focus",
        "false",
        "--",
        &reference,
    ]));
    commands.push(niri_action(&["focus-window", "--id", id]));
    commands
}

pub(super) const NIRI_QUIT: [&str; 5] = ["niri", "msg", "action", "quit", "--skip-confirmation"];

/// Ask niri to quit, ending the session. Blocking.
pub(super) fn niri_quit() -> Result<(), String> {
    let (program, args) = NIRI_QUIT.split_first().unwrap();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start niri: {error}"))?;
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                kill_process_group(child.id());
                let _ = child.wait();
                return Err("niri did not answer".to_string());
            }
            Err(_) => return Err("niri could not be waited for".to_string()),
        }
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    niri_quit_result(status, &stderr)
}

/// niri closes its socket before answering a quit, so `niri msg` reports an
/// empty reply when it worked. Any other failure is a real error.
fn niri_quit_result(status: ExitStatus, stderr: &str) -> Result<(), String> {
    let closed = stderr.contains("error communicating with niri")
        && stderr.contains("EOF while parsing a value at line 1 column 0");
    if status.success() || closed {
        return Ok(());
    }
    let reason = stderr
        .lines()
        .map(|line| line.trim_start_matches("Error:").trim())
        .find(|line| !line.is_empty())
        .unwrap_or("no reason given");
    Err(format!("niri could not quit: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENTS: &str = r#"[
        {"address": "0x1a", "mapped": true, "hidden": false,
         "workspace": {"id": 3, "name": "3"}, "class": "firefox",
         "title": "Mozilla Firefox", "initialClass": "firefox", "pid": 100,
         "focusHistoryID": 2},
        {"address": "0x2b", "mapped": true, "hidden": false,
         "workspace": {"id": 1, "name": "1"}, "class": "com.chmouel.raffi",
         "title": "Raffi", "initialClass": "com.chmouel.raffi", "pid": 4242,
         "focusHistoryID": 0},
        {"address": "0x3c", "mapped": true, "hidden": false,
         "workspace": {"id": -98, "name": "special:magic"}, "class": "Kitty",
         "title": "notes", "initialClass": "kitty", "pid": 101,
         "focusHistoryID": 1},
        {"address": "0x4d", "mapped": false, "hidden": false,
         "workspace": {"id": 2, "name": "2"}, "class": "ghost",
         "title": "unmapped", "initialClass": "ghost", "pid": 102,
         "focusHistoryID": 3},
        {"address": "0x5e", "mapped": true, "hidden": true,
         "workspace": {"id": 2, "name": "2"}, "class": "hidden",
         "title": "hidden", "initialClass": "hidden", "pid": 103,
         "focusHistoryID": 4},
        {"address": "0x92", "mapped": true, "hidden": true,
         "workspace": {"id": 2, "name": "2"}, "class": "foot",
         "title": "tab", "initialClass": "foot", "pid": 107,
         "focusHistoryID": 7, "grouped": ["0x92", "0x93"]},
        {"address": "0x6f", "mapped": true, "hidden": false,
         "workspace": {"id": 2, "name": "2"}, "class": "",
         "title": "", "initialClass": "", "pid": 104,
         "focusHistoryID": 5},
        {"address": "0x70", "mapped": true, "hidden": false,
         "workspace": {"id": 4, "name": "4"}, "class": "slack",
         "title": "Slack", "initialClass": "Slack", "pid": 105,
         "focusHistoryID": -1},
        {"address": "0x81", "mapped": true, "hidden": false,
         "workspace": {"id": 5, "name": "5"}, "class": "mpv",
         "title": "video.mkv", "initialClass": "mpv", "pid": 106,
         "focusHistoryID": 6}
    ]"#;
    const ACTIVE: &str = r#"[
        {"name": "HDMI-A-1", "focused": false,
         "activeWorkspace": {"id": 7, "name": "7"},
         "specialWorkspace": {"id": 0, "name": ""}},
        {"name": "DP-2", "focused": true,
         "activeWorkspace": {"id": 1, "name": "1"},
         "specialWorkspace": {"id": 0, "name": ""}}
    ]"#;

    fn monitor(active: &str, special: &str) -> String {
        format!(
            r#"[{{"focused": true, "activeWorkspace": {active}, "specialWorkspace": {special}}}]"#
        )
    }

    #[test]
    fn test_window_items_filter_and_order() {
        let items = hyprland_items(CLIENTS.as_bytes(), ACTIVE.as_bytes(), 4242).unwrap();
        let ids: Vec<&str> = items.iter().map(|item| item.identity.as_str()).collect();
        assert_eq!(ids, vec!["0x3c", "0x1a", "0x81", "0x92", "0x70"]);

        let kitty = &items[0];
        assert_eq!(kitty.title, "notes");
        assert_eq!(kitty.subtitle, "Kitty on workspace special:magic");
        assert_eq!(kitty.icon_names, vec!["Kitty", "kitty"]);
        assert_eq!(items[4].icon_names, vec!["slack", "Slack"]);
    }

    #[test]
    fn test_window_actions() {
        let items = hyprland_items(CLIENTS.as_bytes(), ACTIVE.as_bytes(), 4242).unwrap();
        let firefox = &items[1];
        assert!(matches!(
            &firefox.primary.as_ref().unwrap().action,
            DeferredAction::Hypr(HyprAction::Focus { address }) if address == "0x1a"
        ));
        let secondary = firefox.secondary.as_ref().unwrap();
        assert!(matches!(
            &secondary.action,
            DeferredAction::Hypr(HyprAction::MoveHere { address, workspace })
                if address == "0x1a" && workspace == "1"
        ));
        assert!(firefox.primary.as_ref().unwrap().confirm.is_none() && secondary.confirm.is_none());
    }

    #[test]
    fn test_move_target_avoids_negative_ids() {
        let named = monitor(
            r#"{"id": -1337, "name": "web"}"#,
            r#"{"id": 0, "name": ""}"#,
        );
        let items = hyprland_items(CLIENTS.as_bytes(), named.as_bytes(), 4242).unwrap();
        assert!(matches!(
            &items[0].secondary.as_ref().unwrap().action,
            DeferredAction::Hypr(HyprAction::MoveHere { workspace, .. }) if workspace == "name:web"
        ));

        let odd = monitor(
            r#"{"id": -1337, "name": "a,b"}"#,
            r#"{"id": 0, "name": ""}"#,
        );
        let items = hyprland_items(CLIENTS.as_bytes(), odd.as_bytes(), 4242).unwrap();
        assert!(items[0].secondary.is_none());
    }

    #[test]
    fn test_move_target_prefers_open_special_workspace() {
        let special = monitor(
            r#"{"id": 1, "name": "1"}"#,
            r#"{"id": -98, "name": "special:magic"}"#,
        );
        let items = hyprland_items(CLIENTS.as_bytes(), special.as_bytes(), 4242).unwrap();
        assert!(matches!(
            &items[1].secondary.as_ref().unwrap().action,
            DeferredAction::Hypr(HyprAction::MoveHere { workspace, .. }) if workspace == "special:magic"
        ));
    }

    #[test]
    fn test_window_items_reject_invalid_json() {
        assert_eq!(
            hyprland_items(b"nope", ACTIVE.as_bytes(), 1).unwrap_err(),
            "hyprctl returned invalid window data"
        );
        assert_eq!(
            hyprland_items(b"[]", b"{}", 1).unwrap_err(),
            "hyprctl returned invalid workspace data"
        );
    }

    // Captured from a nested niri 26.04, plus a Raffi window (pid 4242).
    const NIRI_WINDOWS: &str = r#"[
        {"id":3,"title":"term two","app_id":"kitty","pid":1767294,"workspace_id":1,
         "is_focused":false,"is_floating":true,"is_urgent":false,
         "layout":{"pos_in_scrolling_layout":null,"tile_size":[1903.0,2115.0],
         "window_size":[1903,2115],"tile_pos_in_workspace_view":[0.0,0.0],
         "window_offset_in_tile":[0.0,0.0]},
         "focus_timestamp":{"secs":135924,"nanos":161246648}},
        {"id":4,"title":"Raffi","app_id":"com.chmouel.raffi","pid":4242,"workspace_id":2,
         "is_focused":false,"is_floating":false,"is_urgent":false,
         "focus_timestamp":{"secs":135950,"nanos":0}},
        {"id":2,"title":"term one","app_id":"kitty","pid":1766996,"workspace_id":2,
         "is_focused":true,"is_floating":false,"is_urgent":false,
         "layout":{"pos_in_scrolling_layout":[1,1],"tile_size":[1903.0,2115.0],
         "window_size":[1903,2115],"tile_pos_in_workspace_view":null,
         "window_offset_in_tile":[0.0,0.0]},
         "focus_timestamp":{"secs":135940,"nanos":521592178}}
    ]"#;
    const NIRI_WORKSPACES: &str = r#"[
        {"id":2,"idx":2,"name":null,"output":"winit","is_urgent":false,"is_active":true,
         "is_focused":true,"active_window_id":2},
        {"id":1,"idx":1,"name":"web","output":"winit","is_urgent":false,"is_active":false,
         "is_focused":false,"active_window_id":3},
        {"id":3,"idx":3,"name":null,"output":"winit","is_urgent":false,"is_active":false,
         "is_focused":false,"active_window_id":null}
    ]"#;
    // Older niri: no focus timestamps or layout.
    const NIRI_OLD_WINDOWS: &str = r#"[
        {"id":5,"title":"first","app_id":"foot","pid":10,"workspace_id":1,
         "is_focused":false,"is_floating":false},
        {"id":6,"title":null,"app_id":"org.gnome.Nautilus","pid":11,"workspace_id":1,
         "is_focused":true,"is_floating":false},
        {"id":7,"title":"third","app_id":null,"pid":12,"workspace_id":3,
         "is_focused":false,"is_floating":false},
        {"id":8,"title":null,"app_id":null,"pid":13,"workspace_id":1,
         "is_focused":false,"is_floating":false}
    ]"#;

    fn argvs(action: &CommandAction) -> Vec<Vec<String>> {
        match &action.action {
            DeferredAction::Command(argv) => vec![argv.clone()],
            DeferredAction::CommandBatch(commands) => commands.clone(),
            other => panic!("unexpected action {other:?}"),
        }
    }

    fn niri(args: &str) -> Vec<String> {
        "niri msg action"
            .split(' ')
            .chain(args.split(' '))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn test_compositor_detection_order() {
        let detect = |vars: &[(&str, &str)]| {
            Compositor::detect(|name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            })
        };
        assert_eq!(detect(&[]), None);
        assert_eq!(
            detect(&[("NIRI_SOCKET", "/run/niri.sock")]),
            Some(Compositor::Niri)
        );
        assert_eq!(
            detect(&[
                ("NIRI_SOCKET", "/run/niri.sock"),
                ("HYPRLAND_INSTANCE_SIGNATURE", "abc")
            ]),
            Some(Compositor::Hyprland)
        );
        assert_eq!(
            detect(&[
                ("NIRI_SOCKET", "/run/niri.sock"),
                ("HYPRLAND_INSTANCE_SIGNATURE", "")
            ]),
            Some(Compositor::Niri)
        );
        assert_eq!(detect(&[("NIRI_SOCKET", "")]), None);
    }

    #[test]
    fn test_niri_items_order_and_rows() {
        let items = niri_items(NIRI_WINDOWS.as_bytes(), NIRI_WORKSPACES.as_bytes(), 4242).unwrap();
        let ids: Vec<&str> = items.iter().map(|item| item.identity.as_str()).collect();
        assert_eq!(ids, vec!["2", "3"]);
        assert_eq!(items[0].title, "term one");
        assert_eq!(items[0].subtitle, "kitty on workspace 2");
        assert_eq!(items[1].subtitle, "kitty on workspace web");
        assert_eq!(items[1].icon_names, vec!["kitty"]);

        let items = niri_items(NIRI_OLD_WINDOWS.as_bytes(), NIRI_WORKSPACES.as_bytes(), 1).unwrap();
        let ids: Vec<&str> = items.iter().map(|item| item.identity.as_str()).collect();
        assert_eq!(ids, vec!["6", "5", "7"]);
        assert_eq!(items[0].title, "org.gnome.Nautilus");
        assert_eq!(
            items[0].icon_names,
            vec!["org.gnome.Nautilus", "org.gnome.nautilus"]
        );
        assert_eq!(items[2].subtitle, "Workspace 3");
    }

    #[test]
    fn test_niri_actions() {
        let items = niri_items(NIRI_WINDOWS.as_bytes(), NIRI_WORKSPACES.as_bytes(), 4242).unwrap();
        let term_two = &items[1];
        assert_eq!(
            argvs(term_two.primary.as_ref().unwrap()),
            vec![niri("focus-window --id 3")]
        );
        assert_eq!(
            argvs(term_two.secondary.as_ref().unwrap()),
            vec![
                niri("move-window-to-workspace --window-id 3 --focus false -- 2"),
                niri("focus-window --id 3"),
            ]
        );
    }

    #[test]
    fn test_niri_move_target() {
        let workspaces = |focused: &str| {
            format!(
                r#"[{{"id":1,"idx":1,"name":null,"output":"HDMI-A-1","is_focused":false}},
                    {{"id":2,"idx":1,"name":{focused},"output":"DP-2","is_focused":true}}]"#
            )
        };
        let windows = r#"[{"id":9,"title":"t","app_id":"kitty","pid":1,"workspace_id":1},
                          {"id":10,"title":"u","app_id":"kitty","pid":1,"workspace_id":2}]"#;
        let secondary = |focused: &str, index: usize| {
            let items = niri_items(windows.as_bytes(), workspaces(focused).as_bytes(), 0).unwrap();
            argvs(items[index].secondary.as_ref().unwrap())
        };

        // A name is found on any monitor.
        assert_eq!(
            secondary(r#""web""#, 0)[0],
            niri("move-window-to-workspace --window-id 9 --focus false -- web")
        );
        // An index is per monitor: a window elsewhere changes monitor first.
        for focused in ["null", r#""3""#] {
            assert_eq!(
                secondary(focused, 0),
                vec![
                    niri("move-window-to-monitor --id 9 DP-2"),
                    niri("move-window-to-workspace --window-id 9 --focus false -- 1"),
                    niri("focus-window --id 9"),
                ]
            );
        }
        assert_eq!(
            secondary("null", 1)[0],
            niri("move-window-to-workspace --window-id 10 --focus false -- 1")
        );

        let items = niri_items(windows.as_bytes(), workspaces("null").as_bytes(), 0).unwrap();
        assert_eq!(items[0].subtitle, "kitty on workspace 1 (HDMI-A-1)");

        let unfocused = r#"[{"id":1,"idx":1,"name":null,"output":"DP-2","is_focused":false}]"#;
        let items = niri_items(windows.as_bytes(), unfocused.as_bytes(), 0).unwrap();
        assert!(items[0].secondary.is_none());
    }

    #[test]
    fn test_niri_items_reject_invalid_json() {
        assert_eq!(
            niri_items(b"nope", NIRI_WORKSPACES.as_bytes(), 1).unwrap_err(),
            "niri returned invalid window data"
        );
        assert_eq!(
            niri_items(b"[]", b"{}", 1).unwrap_err(),
            "niri returned invalid workspace data"
        );
    }

    /// Focuses and moves a window in a niri session; never run by default.
    /// Point NIRI_SOCKET at a throwaway nested niri with two windows, the
    /// focused one on another workspace than `RAFFI_NIRI_TEST_ID`:
    /// `RAFFI_NIRI_TEST_ID=3 cargo test --all-features -- --ignored live_niri`
    #[test]
    #[ignore]
    fn live_niri_actions() {
        let id = std::env::var("RAFFI_NIRI_TEST_ID").unwrap();
        let items = Compositor::Niri.load_windows().unwrap();
        let item = items.iter().find(|item| item.identity == id).unwrap();
        let run = |action: &CommandAction| {
            super::super::actions::run_deferred(&std::sync::Mutex::new(Some(action.action.clone())))
        };
        run(item.secondary.as_ref().unwrap()).unwrap();
        let items = Compositor::Niri.load_windows().unwrap();
        assert_eq!(items[0].identity, id, "the moved window has the focus");
        run(items[1].primary.as_ref().unwrap()).unwrap();
        let items = Compositor::Niri.load_windows().unwrap();
        assert_ne!(items[0].identity, id, "the other window has the focus");
    }

    #[test]
    fn test_niri_quit_result() {
        use std::os::unix::process::ExitStatusExt;
        let failed = ExitStatus::from_raw(1 << 8);
        // Captured from niri 26.04 after a successful quit.
        let quit = "Error: error communicating with niri\n\nCaused by:\n    EOF while parsing a value at line 1 column 0\n";
        assert!(niri_quit_result(failed, quit).is_ok());
        assert!(niri_quit_result(ExitStatus::from_raw(0), "").is_ok());
        assert_eq!(
            niri_quit_result(
                failed,
                "Error: error connecting to the niri socket\n\nCaused by:\n    No such file or directory (os error 2)\n"
            )
            .unwrap_err(),
            "niri could not quit: error connecting to the niri socket"
        );
        assert_eq!(
            niri_quit_result(
                failed,
                "Error: error communicating with niri\n\nCaused by:\n    expected value at line 1 column 1\n"
            )
            .unwrap_err(),
            "niri could not quit: error communicating with niri"
        );
    }
}
