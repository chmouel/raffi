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

pub(super) use crate::compositor::Compositor;

/// What the window switcher lists; Ctrl+Tab switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum View {
    #[default]
    Windows,
    Workspaces,
}

impl View {
    pub(super) fn cycle(self) -> Self {
        match self {
            Self::Windows => Self::Workspaces,
            Self::Workspaces => Self::Windows,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Workspaces => "Workspaces",
        }
    }
}

impl Compositor {
    /// List the windows, most recently focused first. Blocking: run it off
    /// the UI thread.
    pub(super) fn load_windows(self, keep_open: bool) -> Result<Vec<CommandItem>, String> {
        self.load_view(View::Windows, keep_open)
    }

    /// List the windows or the workspaces. Blocking: run it off the UI
    /// thread.
    pub(super) fn load_view(self, view: View, keep_open: bool) -> Result<Vec<CommandItem>, String> {
        let program = self.program();
        let run = |args: &[&str]| {
            let mut command = Command::new(program);
            command.args(args);
            run_bounded(command, TIMEOUT, MAX_OUTPUT).map_err(|error| format!("{program}: {error}"))
        };
        let own_pid = std::process::id();
        let mut items = match (self, view) {
            (Self::Hyprland, View::Windows) => hyprland_items(
                &run(&["clients", "-j"])?,
                &run(&["monitors", "-j"])?,
                own_pid,
            )?,
            (Self::Hyprland, View::Workspaces) => hyprland_workspace_items(
                &run(&["workspaces", "-j"])?,
                &run(&["monitors", "-j"])?,
                &run(&["clients", "-j"])?,
                own_pid,
            )?,
            (Self::Niri, View::Windows) => niri_items(
                &run(&["msg", "--json", "windows"])?,
                &run(&["msg", "--json", "workspaces"])?,
                own_pid,
            )?,
            (Self::Niri, View::Workspaces) => niri_workspace_items(
                &run(&["msg", "--json", "windows"])?,
                &run(&["msg", "--json", "workspaces"])?,
                own_pid,
            )?,
            (Self::Sway, View::Windows) => sway_items(
                &run(&["-t", "get_tree", "-r"])?,
                &run(&["-t", "get_workspaces", "-r"])?,
                own_pid,
            )?,
            (Self::Sway, View::Workspaces) => sway_workspace_items(
                &run(&["-t", "get_tree", "-r"])?,
                &run(&["-t", "get_workspaces", "-r"])?,
                own_pid,
            )?,
        };
        if keep_open {
            for item in &mut items {
                if let Some(primary) = &mut item.primary {
                    primary.keep_open = true;
                }
                if let Some(secondary) = &mut item.secondary {
                    secondary.keep_open = true;
                }
            }
        }
        Ok(items)
    }
}

/// How long to wait for a browser window to show the tab it switched to.
const RAISE_WAIT: Duration = Duration::from_secs(1);
const RAISE_POLL: Duration = Duration::from_millis(100);

/// A window with the process owning it, for raising a browser window.
#[derive(Debug)]
pub(super) struct ProcessWindow {
    pid: Option<i64>,
    title: String,
    /// The app ID, or the class of an Xwayland window.
    app: String,
    focus: DeferredAction,
}

impl Compositor {
    fn process_windows(self) -> Result<Vec<ProcessWindow>, String> {
        let program = self.program();
        let mut command = Command::new(program);
        command.args(match self {
            Self::Hyprland => &["clients", "-j"][..],
            Self::Niri => &["msg", "--json", "windows"][..],
            Self::Sway => &["-t", "get_tree", "-r"][..],
        });
        let output = run_bounded(command, TIMEOUT, MAX_OUTPUT)
            .map_err(|error| format!("{program}: {error}"))?;
        process_windows(self, &output)
    }
}

/// The windows in the output of `hyprctl clients -j`, `niri msg --json
/// windows` or `swaymsg -t get_tree -r`, the most recently focused first
/// where the compositor tells.
pub(super) fn process_windows(
    compositor: Compositor,
    output: &[u8],
) -> Result<Vec<ProcessWindow>, String> {
    let invalid = |_| format!("{} returned invalid window data", compositor.program());
    Ok(match compositor {
        Compositor::Hyprland => {
            let mut clients = serde_json::from_slice::<Vec<Client>>(output).map_err(invalid)?;
            clients.sort_by_key(|client| (client.focus_history_id < 0, client.focus_history_id));
            clients
                .into_iter()
                .filter(|client| client.mapped && valid_address(&client.address))
                .map(|client| ProcessWindow {
                    pid: Some(client.pid),
                    title: client.title,
                    app: if client.class.is_empty() {
                        client.initial_class
                    } else {
                        client.class
                    },
                    focus: DeferredAction::Hypr(HyprAction::Focus {
                        address: client.address,
                    }),
                })
                .collect()
        }
        Compositor::Niri => {
            let mut windows = serde_json::from_slice::<Vec<NiriWindow>>(output).map_err(invalid)?;
            windows.sort_by_key(|window| Reverse((window.is_focused, window.focus_timestamp)));
            windows
                .into_iter()
                .map(|window| ProcessWindow {
                    pid: window.pid,
                    title: window.title.unwrap_or_default(),
                    app: window.app_id.unwrap_or_default(),
                    focus: DeferredAction::Command(niri_action(&[
                        "focus-window",
                        "--id",
                        &window.id.to_string(),
                    ])),
                })
                .collect()
        }
        Compositor::Sway => {
            let tree: SwayNode = serde_json::from_slice(output).map_err(invalid)?;
            let mut windows = Vec::new();
            sway_windows(&tree, None, None, &mut windows);
            windows
                .into_iter()
                .map(|window| ProcessWindow {
                    pid: window.node.pid,
                    title: window.node.name.clone().unwrap_or_default(),
                    app: window.node.app().0.to_string(),
                    focus: DeferredAction::Command(sway_command(
                        &window.node.id.to_string(),
                        "focus",
                    )),
                })
                .collect()
        }
    })
}

/// The window of process `pid` whose title starts with `title`. Once the
/// wait is over, a process with a single window gets that one.
pub(super) fn pick_process_window<'a>(
    windows: &'a [ProcessWindow],
    pid: u32,
    title: &str,
    settled: bool,
) -> Option<&'a DeferredAction> {
    let mut owned = windows
        .iter()
        .filter(|window| window.pid == Some(i64::from(pid)));
    let title = title.trim();
    if let Some(window) = owned
        .clone()
        .find(|window| !title.is_empty() && window.title.starts_with(title))
    {
        return Some(&window.focus);
    }
    match (settled, owned.next(), owned.next()) {
        (true, Some(window), None) => Some(&window.focus),
        _ => None,
    }
}

/// Focus the window of process `pid` showing `title`, waiting briefly for
/// its title to follow a tab switch. Does nothing outside Hyprland, niri and
/// sway. Blocking: run it off the UI thread.
pub(super) fn raise_process_window(pid: u32, title: &str) -> Result<(), String> {
    let Some(compositor) = Compositor::current() else {
        return Ok(());
    };
    let deadline = Instant::now() + RAISE_WAIT;
    loop {
        let windows = compositor.process_windows()?;
        let settled = Instant::now() >= deadline;
        if let Some(focus) = pick_process_window(&windows, pid, title, settled) {
            return focus.run();
        }
        if settled {
            crate::debug_log!("browser tabs: no window of pid {pid} shows {title:?}");
            return Ok(());
        }
        std::thread::sleep(RAISE_POLL);
    }
}

/// The first window of one of `apps`, matched without case and also as the
/// last part of a reverse-DNS app ID, so `slack` finds `Slack` and
/// `com.slack.Slack`.
pub(super) fn pick_app_window<'a>(
    windows: &'a [ProcessWindow],
    apps: &[&str],
) -> Option<&'a DeferredAction> {
    let apps: Vec<(String, String)> = apps
        .iter()
        .map(|app| (app.to_lowercase(), format!(".{}", app.to_lowercase())))
        .collect();
    windows
        .iter()
        .find(|window| {
            let name = window.app.to_lowercase();
            apps.iter()
                .any(|(app, suffix)| name == *app || name.ends_with(suffix.as_str()))
        })
        .map(|window| &window.focus)
}

/// The focus action for the most recent window of one of `apps`, if any.
/// Blocking: run it off the UI thread.
pub(super) fn find_app_window(
    compositor: Compositor,
    apps: &[String],
) -> Result<Option<DeferredAction>, String> {
    let windows = compositor.process_windows()?;
    let apps: Vec<&str> = apps.iter().map(String::as_str).collect();
    Ok(pick_app_window(&windows, &apps).cloned())
}

/// Focus the most recent window of one of `apps`, waiting up to `wait` for
/// it to appear, as when the app is starting. Does nothing outside
/// Hyprland, niri and sway. Blocking: run it off the UI thread.
pub(super) fn raise_app_window(apps: &[&str], wait: Duration) -> Result<(), String> {
    let Some(compositor) = Compositor::current() else {
        return Ok(());
    };
    let deadline = Instant::now() + wait;
    loop {
        let windows = compositor.process_windows()?;
        if let Some(focus) = pick_app_window(&windows, apps) {
            return focus.run();
        }
        if Instant::now() >= deadline {
            crate::debug_log!("windows: no {apps:?} window to raise");
            return Ok(());
        }
        std::thread::sleep(RAISE_POLL);
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
    /// States worth showing, most notable first; the first is the badge.
    flags: Vec<&'static str>,
    focus: DeferredAction,
    move_here: Option<DeferredAction>,
    close: DeferredAction,
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
    ];
    if !window.flags.is_empty() {
        details.push(("State".to_string(), window.flags.join(", ")));
    }
    details.push(("Enter".to_string(), "Focus the window".to_string()));
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
    details.push(("Ctrl+D".to_string(), "Close the window".to_string()));
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
        badge: window.flags.first().map(|flag| flag.to_string()),
        domains: Vec::new(),
        dimmed: false,
        sort_priority: 0,
        typed_only: false,
        close: Some(CommandAction {
            action: window.close,
            confirm: None,
            keep_open: true,
            progress: Some("Closing...".to_string()),
        }),
        group: None,
        pinned: false,
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

/// What a workspace row needs, whatever the compositor.
struct WorkspaceRow<'a> {
    name: String,
    /// Shown when there are several outputs.
    output: Option<&'a str>,
    /// Titles and apps of its windows, the most recently focused first.
    windows: Vec<(&'a str, &'a str)>,
    /// "Current" for the focused one, "Visible" for one shown elsewhere.
    state: Option<&'static str>,
    /// None when the compositor cannot be asked to switch to it.
    switch: Option<(DeferredAction, &'static str)>,
    /// Moves the last focused window here and follows it, with its title.
    move_here: Option<(&'a str, DeferredAction)>,
}

const WORKSPACE_ICONS: [&str; 3] = ["workspace-switcher", "cs-workspaces", "view-grid"];

fn workspace_item(row: WorkspaceRow) -> CommandItem {
    let count = match row.windows.len() {
        0 => "No windows".to_string(),
        1 => "1 window".to_string(),
        count => format!("{count} windows"),
    };
    let last = row.windows.first().map(|(title, app)| {
        if title.is_empty() {
            app.to_string()
        } else {
            title.to_string()
        }
    });
    let subtitle = row
        .output
        .map(|output| format!("on {output}"))
        .into_iter()
        .chain([count.clone()])
        .chain(last.clone())
        .collect::<Vec<_>>()
        .join(" · ");
    let mut details = vec![("Windows".to_string(), count)];
    if let Some(last) = last {
        details.push(("Last window".to_string(), last));
    }
    if let Some(output) = row.output {
        details.push(("Output".to_string(), output.to_string()));
    }
    let action = |action| CommandAction {
        action,
        confirm: None,
        keep_open: false,
        progress: None,
    };
    let (primary, unavailable) = match row.switch {
        Some((switch, label)) => {
            details.push(("Enter".to_string(), label.to_string()));
            (Some(action(switch)), None)
        }
        None => (
            None,
            Some("Raffi cannot switch to a workspace with this name".to_string()),
        ),
    };
    let secondary = row
        .move_here
        .filter(|_| primary.is_some())
        .map(|(title, move_here)| {
            details.push((
                "Ctrl+Enter".to_string(),
                format!("Move \u{201c}{title}\u{201d} here and follow it"),
            ));
            action(move_here)
        });
    let match_text = std::iter::once(row.name.as_str())
        .chain(row.windows.iter().flat_map(|(title, app)| [*title, *app]))
        .collect::<Vec<_>>()
        .join(" ");
    CommandItem {
        title: format!("Workspace {}", row.name),
        subtitle,
        match_text: format!("workspace {match_text}"),
        icon_names: WORKSPACE_ICONS
            .iter()
            .map(|icon| icon.to_string())
            .collect(),
        icon: None,
        identity: format!("workspace:{}", row.name),
        unavailable,
        primary,
        secondary,
        details,
        badge: row.state.map(str::to_string),
        domains: Vec::new(),
        dimmed: row.windows.is_empty() && row.state.is_none(),
        sort_priority: 0,
        typed_only: false,
        close: None,
        group: None,
        pinned: false,
    }
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
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    fullscreen: HyprFullscreen,
    #[serde(default)]
    xwayland: bool,
    #[serde(default)]
    monitor: Option<i64>,
}

fn mapped_default() -> bool {
    true
}

/// A boolean before Hyprland 0.42, then 1 for maximized, 2 for fullscreen
/// and 3 for both.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HyprFullscreen {
    Old(bool),
    Mode(u8),
}

impl Default for HyprFullscreen {
    fn default() -> Self {
        Self::Mode(0)
    }
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
    id: i64,
    #[serde(default)]
    name: String,
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
    let output = |client: &Client| {
        monitors
            .iter()
            .find(|monitor| Some(monitor.id) == client.monitor && monitors.len() > 1)
            .map(|monitor| monitor.name.clone())
    };
    Ok(clients
        .into_iter()
        .map(|client| {
            let output = output(&client);
            hyprland_window(client, target.as_deref(), output.as_deref())
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct HyprWorkspace {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    monitor: String,
}

/// Build the workspace rows from `hyprctl workspaces -j`, `monitors -j` and
/// `clients -j` output, numbered workspaces first, then named, then special
/// ones. The windows of `own_pid` are left out.
pub(super) fn hyprland_workspace_items(
    workspaces: &[u8],
    monitors: &[u8],
    clients: &[u8],
    own_pid: u32,
) -> Result<Vec<CommandItem>, String> {
    let invalid = |_| "hyprctl returned invalid workspace data".to_string();
    let mut workspaces: Vec<HyprWorkspace> = serde_json::from_slice(workspaces).map_err(invalid)?;
    let monitors: Vec<Monitor> = serde_json::from_slice(monitors).map_err(invalid)?;
    let mut clients: Vec<Client> = serde_json::from_slice(clients)
        .map_err(|_| "hyprctl returned invalid window data".to_string())?;
    clients.retain(|client| {
        client.mapped
            && (!client.hidden || !client.grouped.is_empty())
            && client.pid != i64::from(own_pid)
            && !(client.title.is_empty() && client.class.is_empty())
            && valid_address(&client.address)
    });
    clients.sort_by_key(|client| (client.focus_history_id < 0, client.focus_history_id));
    workspaces.sort_by(|a, b| {
        let key = |workspace: &HyprWorkspace| {
            (
                workspace.name.starts_with("special:"),
                workspace.id < 0,
                workspace.id,
            )
        };
        key(a).cmp(&key(b)).then_with(|| a.name.cmp(&b.name))
    });
    let state = |workspace: &HyprWorkspace| {
        monitors.iter().find_map(|monitor| {
            let special = monitor
                .special_workspace
                .as_ref()
                .filter(|special| !special.name.is_empty());
            let on_top = special.map_or(monitor.active_workspace.id, |special| special.id);
            if workspace.id == on_top && monitor.focused {
                Some("Current")
            } else if workspace.id == on_top || workspace.id == monitor.active_workspace.id {
                Some("Visible")
            } else {
                None
            }
        })
    };
    let recent = clients.first();
    Ok(workspaces
        .iter()
        .map(|workspace| {
            let selector = move_target(&Workspace {
                id: workspace.id,
                name: workspace.name.clone(),
            });
            let switch = match workspace.name.strip_prefix("special:") {
                Some(name) => Some((
                    DeferredAction::Hypr(HyprAction::ToggleSpecial {
                        name: name.to_string(),
                    }),
                    "Show or hide this special workspace",
                )),
                None => selector.clone().map(|workspace| {
                    (
                        DeferredAction::Hypr(HyprAction::Workspace { workspace }),
                        "Switch to this workspace",
                    )
                }),
            };
            let move_here = recent
                .filter(|client| client.workspace.id != workspace.id)
                .zip(selector)
                .map(|(client, selector)| {
                    (
                        client.title.as_str(),
                        DeferredAction::Hypr(HyprAction::MoveTo {
                            address: client.address.clone(),
                            workspace: selector,
                        }),
                    )
                });
            workspace_item(WorkspaceRow {
                name: if workspace.name.is_empty() {
                    workspace.id.to_string()
                } else {
                    workspace.name.clone()
                },
                output: Some(workspace.monitor.as_str())
                    .filter(|output| monitors.len() > 1 && !output.is_empty()),
                windows: clients
                    .iter()
                    .filter(|client| client.workspace.id == workspace.id)
                    .map(|client| (client.title.as_str(), client.class.as_str()))
                    .collect(),
                state: state(workspace),
                switch,
                move_here,
            })
        })
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

fn hyprland_window(client: Client, target: Option<&str>, output: Option<&str>) -> CommandItem {
    let mut workspace = if client.workspace.name.is_empty() {
        client.workspace.id.to_string()
    } else {
        client.workspace.name.clone()
    };
    if let Some(output) = output.filter(|output| !output.is_empty()) {
        workspace = format!("{workspace} ({output})");
    }
    let mut flags = Vec::new();
    match client.fullscreen {
        HyprFullscreen::Old(true) | HyprFullscreen::Mode(2..) => flags.push("Fullscreen"),
        HyprFullscreen::Mode(1) => flags.push("Maximized"),
        _ => {}
    }
    for (set, flag) in [
        (client.pinned, "Pinned"),
        (client.floating, "Floating"),
        (!client.grouped.is_empty(), "Grouped"),
        (client.xwayland, "XWayland"),
    ] {
        if set {
            flags.push(flag);
        }
    }
    window_item(Window {
        flags,
        close: DeferredAction::Hypr(HyprAction::Close {
            address: client.address.clone(),
        }),
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
    #[serde(default)]
    is_floating: bool,
    #[serde(default)]
    is_urgent: bool,
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
    #[serde(default)]
    is_active: bool,
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
    let flags = [
        (window.is_urgent, "Urgent"),
        (window.is_floating, "Floating"),
    ]
    .into_iter()
    .filter_map(|(set, flag)| set.then_some(flag))
    .collect();
    window_item(Window {
        flags,
        close: DeferredAction::Command(niri_action(&["close-window", "--id", &id])),
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

/// Build the workspace rows from `niri msg --json windows` and `niri msg
/// --json workspaces` output, by output then index, leaving out the windows
/// of `own_pid`.
pub(super) fn niri_workspace_items(
    windows: &[u8],
    workspaces: &[u8],
    own_pid: u32,
) -> Result<Vec<CommandItem>, String> {
    let mut windows: Vec<NiriWindow> = serde_json::from_slice(windows)
        .map_err(|_| "niri returned invalid window data".to_string())?;
    let mut workspaces: Vec<NiriWorkspace> = serde_json::from_slice(workspaces)
        .map_err(|_| "niri returned invalid workspace data".to_string())?;
    windows.retain(|window| {
        window.pid != Some(i64::from(own_pid))
            && !(window.title.as_deref().unwrap_or_default().is_empty()
                && window.app_id.as_deref().unwrap_or_default().is_empty())
    });
    windows.sort_by_key(|window| Reverse((window.is_focused, window.focus_timestamp)));
    workspaces.sort_by(|a, b| (&a.output, a.idx).cmp(&(&b.output, b.idx)));
    let focused_output = workspaces
        .iter()
        .find(|workspace| workspace.is_focused)
        .and_then(|workspace| workspace.output.clone());
    let several_outputs = workspaces
        .iter()
        .any(|workspace| workspace.output != workspaces[0].output);
    let recent = windows.first();
    Ok(workspaces
        .iter()
        .map(|workspace| {
            let reference = niri_reference(workspace);
            let mut switch = Vec::new();
            if let Some(output) = workspace
                .output
                .as_deref()
                .filter(|&output| Some(output) != focused_output.as_deref())
            {
                switch.push(niri_action(&["focus-monitor", output]));
            }
            switch.push(niri_action(&["focus-workspace", &reference]));
            let move_here = recent
                .filter(|window| window.workspace_id != Some(workspace.id))
                .map(|window| {
                    let output = workspaces
                        .iter()
                        .find(|other| Some(other.id) == window.workspace_id)
                        .and_then(|other| other.output.as_deref());
                    (
                        window.title.as_deref().unwrap_or_default(),
                        DeferredAction::CommandBatch(niri_move_here(
                            &window.id.to_string(),
                            output,
                            workspace,
                        )),
                    )
                });
            workspace_item(WorkspaceRow {
                name: workspace
                    .name
                    .clone()
                    .unwrap_or_else(|| workspace.idx.to_string()),
                output: workspace.output.as_deref().filter(|_| several_outputs),
                windows: windows
                    .iter()
                    .filter(|window| window.workspace_id == Some(workspace.id))
                    .map(|window| {
                        (
                            window.title.as_deref().unwrap_or_default(),
                            window.app_id.as_deref().unwrap_or_default(),
                        )
                    })
                    .collect(),
                state: if workspace.is_focused {
                    Some("Current")
                } else if workspace.is_active {
                    Some("Visible")
                } else {
                    None
                },
                switch: Some((
                    DeferredAction::CommandBatch(switch),
                    "Switch to this workspace",
                )),
                move_here,
            })
        })
        .collect())
}

/// How niri commands name `workspace`: its name, unless niri would read it
/// as an index, else its index on its output.
fn niri_reference(workspace: &NiriWorkspace) -> String {
    match &workspace.name {
        Some(name) if name.parse::<i32>().is_err() => name.clone(),
        _ => workspace.idx.to_string(),
    }
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
    let reference = niri_reference(focused);
    if reference == focused.idx.to_string() {
        if let Some(target) = focused
            .output
            .as_deref()
            .filter(|&target| Some(target) != output)
        {
            commands.push(niri_action(&["move-window-to-monitor", "--id", id, target]));
        }
    }
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

#[derive(Debug, Deserialize)]
struct SwayNode {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    window_properties: Option<SwayWindowProperties>,
    #[serde(default)]
    nodes: Vec<SwayNode>,
    #[serde(default)]
    floating_nodes: Vec<SwayNode>,
    /// Child ids, most recently focused first.
    #[serde(default)]
    focus: Vec<i64>,
    #[serde(default)]
    fullscreen_mode: u8,
    #[serde(default)]
    sticky: bool,
    #[serde(default)]
    urgent: bool,
    /// `xdg_shell` or `xwayland`.
    #[serde(default)]
    shell: Option<String>,
}

/// Set for Xwayland windows.
#[derive(Debug, Deserialize)]
struct SwayWindowProperties {
    #[serde(default)]
    class: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SwayWorkspace {
    name: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    output: Option<String>,
}

const SWAY_SCRATCHPAD: &str = "__i3_scratch";

struct SwayWindow<'a> {
    node: &'a SwayNode,
    workspace: Option<&'a str>,
    output: Option<&'a str>,
}

impl SwayNode {
    /// The app ID, or the class of an Xwayland window.
    fn app(&self) -> (&str, &'static str) {
        match self.app_id.as_deref().filter(|app_id| !app_id.is_empty()) {
            Some(app_id) => (app_id, "App ID"),
            None => (
                self.window_properties
                    .as_ref()
                    .and_then(|properties| properties.class.as_deref())
                    .unwrap_or_default(),
                "Class",
            ),
        }
    }
}

/// Build the window rows from `swaymsg -t get_tree -r` and `swaymsg -t
/// get_workspaces -r` output, leaving out the windows of `own_pid`.
pub(super) fn sway_items(
    tree: &[u8],
    workspaces: &[u8],
    own_pid: u32,
) -> Result<Vec<CommandItem>, String> {
    let tree: SwayNode = serde_json::from_slice(tree)
        .map_err(|_| "sway returned invalid window data".to_string())?;
    let workspaces: Vec<SwayWorkspace> = serde_json::from_slice(workspaces)
        .map_err(|_| "sway returned invalid workspace data".to_string())?;
    let target = workspaces
        .iter()
        .find(|workspace| workspace.focused)
        .and_then(|workspace| sway_quote(&workspace.name));
    let several_outputs = workspaces
        .iter()
        .any(|workspace| workspace.output != workspaces[0].output);

    let mut windows = Vec::new();
    sway_windows(&tree, None, None, &mut windows);
    Ok(windows
        .into_iter()
        .filter(|window| window.node.pid != Some(i64::from(own_pid)))
        .map(|window| sway_window(window, target.as_deref(), several_outputs))
        .collect())
}

/// Build the workspace rows from `swaymsg -t get_tree -r` and `swaymsg -t
/// get_workspaces -r` output, in sway's order, leaving out the windows of
/// `own_pid`.
pub(super) fn sway_workspace_items(
    tree: &[u8],
    workspaces: &[u8],
    own_pid: u32,
) -> Result<Vec<CommandItem>, String> {
    let tree: SwayNode = serde_json::from_slice(tree)
        .map_err(|_| "sway returned invalid window data".to_string())?;
    let workspaces: Vec<SwayWorkspace> = serde_json::from_slice(workspaces)
        .map_err(|_| "sway returned invalid workspace data".to_string())?;
    let several_outputs = workspaces
        .iter()
        .any(|workspace| workspace.output != workspaces[0].output);
    let mut windows = Vec::new();
    sway_windows(&tree, None, None, &mut windows);
    windows.retain(|window| window.node.pid != Some(i64::from(own_pid)));
    let recent = windows.first();
    Ok(workspaces
        .iter()
        .map(|workspace| {
            let quoted = sway_quote(&workspace.name);
            let switch = quoted.as_ref().map(|quoted| {
                (
                    DeferredAction::Command(vec![
                        "swaymsg".to_string(),
                        format!("workspace --no-auto-back-and-forth {quoted}"),
                    ]),
                    "Switch to this workspace",
                )
            });
            let move_here = recent
                .filter(|window| window.workspace != Some(workspace.name.as_str()))
                .zip(quoted)
                .map(|(window, quoted)| {
                    let id = window.node.id.to_string();
                    (
                        window.node.name.as_deref().unwrap_or_default(),
                        DeferredAction::CommandBatch(vec![
                            sway_command(
                                &id,
                                &format!(
                                    "move --no-auto-back-and-forth container to workspace {quoted}"
                                ),
                            ),
                            sway_command(&id, "focus"),
                        ]),
                    )
                });
            workspace_item(WorkspaceRow {
                name: workspace.name.clone(),
                output: workspace.output.as_deref().filter(|_| several_outputs),
                windows: windows
                    .iter()
                    .filter(|window| window.workspace == Some(workspace.name.as_str()))
                    .map(|window| {
                        (
                            window.node.name.as_deref().unwrap_or_default(),
                            window.node.app().0,
                        )
                    })
                    .collect(),
                state: if workspace.focused {
                    Some("Current")
                } else if workspace.visible {
                    Some("Visible")
                } else {
                    None
                },
                switch,
                move_here,
            })
        })
        .collect())
}

/// Collect the windows under `node`, visiting the children of each container
/// most recently focused first. Sway keeps that order per container, not
/// globally, so the windows of a workspace stay together even when one of
/// them was focused after a window of the next workspace.
fn sway_windows<'a>(
    node: &'a SwayNode,
    workspace: Option<&'a str>,
    output: Option<&'a str>,
    windows: &mut Vec<SwayWindow<'a>>,
) {
    let (workspace, output) = match node.kind.as_str() {
        "output" => (workspace, node.name.as_deref()),
        "workspace" => (node.name.as_deref(), output),
        _ => (workspace, output),
    };
    if matches!(node.kind.as_str(), "con" | "floating_con")
        && node.pid.is_some()
        && !(node.name.as_deref().unwrap_or_default().is_empty() && node.app().0.is_empty())
    {
        windows.push(SwayWindow {
            node,
            workspace,
            output,
        });
    }
    let children = || node.nodes.iter().chain(&node.floating_nodes);
    let focused = node
        .focus
        .iter()
        .filter_map(|id| children().find(|child| child.id == *id));
    // The scratchpad is missing from the root focus list.
    let rest = children().filter(|child| !node.focus.contains(&child.id));
    for child in focused.chain(rest) {
        sway_windows(child, workspace, output, windows);
    }
}

fn sway_window(window: SwayWindow, target: Option<&str>, several_outputs: bool) -> CommandItem {
    let node = window.node;
    let mut label = match window.workspace {
        Some(SWAY_SCRATCHPAD) => "scratchpad".to_string(),
        Some(name) => name.to_string(),
        None => String::new(),
    };
    if let Some(output) = window
        .output
        .filter(|_| several_outputs && window.workspace != Some(SWAY_SCRATCHPAD))
    {
        label = format!("{label} ({output})");
    }
    let id = node.id.to_string();
    let (app, app_label) = node.app();
    let focus = sway_command(&id, "focus");
    let flags = [
        (node.urgent, "Urgent"),
        (node.fullscreen_mode > 0, "Fullscreen"),
        (node.sticky, "Sticky"),
        (node.kind == "floating_con", "Floating"),
        (node.shell.as_deref() == Some("xwayland"), "XWayland"),
    ]
    .into_iter()
    .filter_map(|(set, flag)| set.then_some(flag))
    .collect();
    window_item(Window {
        flags,
        close: DeferredAction::Command(sway_command(&id, "kill")),
        title: node.name.clone().unwrap_or_default(),
        icon_names: icon_names(&[app]),
        app: app.to_string(),
        app_label,
        workspace: label,
        move_here: target.map(|target| {
            DeferredAction::CommandBatch(vec![
                sway_command(
                    &id,
                    &format!("move --no-auto-back-and-forth container to workspace {target}"),
                ),
                focus.clone(),
            ])
        }),
        focus: DeferredAction::Command(focus),
        identity: id,
    })
}

/// `swaymsg` running `command` on the window with container `id`.
fn sway_command(id: &str, command: &str) -> Vec<String> {
    vec!["swaymsg".to_string(), format!("[con_id={id}] {command}")]
}

/// Quote a workspace `name` as the last argument of a sway command, or None
/// when sway cannot read it back. Sway splits commands on `;` and `,` and
/// arguments on spaces outside quotes, drops quotes not escaped by a
/// backslash but keeps every backslash, then expands `$` variables, leaving
/// `$` alone after a single backslash and turning `$$` into `$`. A `"` goes
/// in single quotes. Keywords such as `next` and names ending in an odd
/// number of backslashes, which would escape the closing quote, have no
/// quoted form.
fn sway_quote(name: &str) -> Option<String> {
    const KEYWORDS: [&str; 7] = [
        "next",
        "prev",
        "next_on_output",
        "prev_on_output",
        "current",
        "back_and_forth",
        "number",
    ];
    if name.is_empty()
        || KEYWORDS
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in name.chars() {
        match c {
            '"' if backslashes % 2 == 0 => quoted.push_str(r#""'"'""#),
            '$' if backslashes != 1 => quoted.push_str("$$"),
            c => quoted.push(c),
        }
        backslashes = if c == '\\' { backslashes + 1 } else { 0 };
    }
    quoted.push('"');
    (backslashes % 2 == 0).then_some(quoted)
}

pub(super) const NIRI_QUIT: [&str; 5] = ["niri", "msg", "action", "quit", "--skip-confirmation"];
pub(super) const SWAY_EXIT: [&str; 2] = ["swaymsg", "exit"];

/// Ask niri to quit, ending the session. Blocking.
pub(super) fn niri_quit() -> Result<(), String> {
    quit_session(&NIRI_QUIT, "niri", niri_closed)
}

/// Ask sway to exit, ending the session. Blocking.
pub(super) fn sway_exit() -> Result<(), String> {
    quit_session(&SWAY_EXIT, "sway", sway_closed)
}

/// Run `argv`, which ends the `compositor` session. The compositor may close
/// its socket before answering; `closed` recognizes that error output.
fn quit_session(argv: &[&str], compositor: &str, closed: fn(&str) -> bool) -> Result<(), String> {
    let (program, args) = argv.split_first().unwrap();
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("cannot start {program}: {error}"))?;
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                kill_process_group(child.id());
                let _ = child.wait();
                return Err(format!("{program} did not answer"));
            }
            Err(_) => return Err(format!("{program} could not be waited for")),
        }
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    quit_result(compositor, status, &stderr, closed)
}

/// niri closes its socket before answering a quit, so `niri msg` reports an
/// empty reply when it worked.
fn niri_closed(stderr: &str) -> bool {
    stderr.contains("error communicating with niri")
        && stderr.contains("EOF while parsing a value at line 1 column 0")
}

/// sway closes its socket before answering `exit`.
fn sway_closed(stderr: &str) -> bool {
    stderr.contains("Unable to receive IPC response")
}

/// Any failure other than the closed socket is a real error.
fn quit_result(
    compositor: &str,
    status: ExitStatus,
    stderr: &str,
    closed: fn(&str) -> bool,
) -> Result<(), String> {
    if status.success() || closed(stderr) {
        return Ok(());
    }
    // swaymsg prefixes its errors with a time and a source location.
    let reason = stderr
        .lines()
        .map(|line| {
            let line = line.rsplit_once("] ").map_or(line, |(_, message)| message);
            line.trim_start_matches("Error:").trim()
        })
        .find(|line| !line.is_empty())
        .unwrap_or("no reason given");
    Err(format!("{compositor} could not quit: {reason}"))
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
    fn test_hyprland_rows_show_state_monitor_and_close() {
        let clients = r#"[
            {"address":"0x1","workspace":{"id":1,"name":"1"},"class":"mpv","title":"a","pid":1,
             "floating":true,"pinned":true,"xwayland":true,"fullscreen":0,"monitor":1},
            {"address":"0x2","workspace":{"id":2,"name":"2"},"class":"foot","title":"b","pid":2,
             "fullscreen":1,"grouped":["0x2","0x3"],"monitor":0},
            {"address":"0x3","workspace":{"id":2,"name":"2"},"class":"foot","title":"c","pid":3,
             "fullscreen":true},
            {"address":"0x4","workspace":{"id":2,"name":"2"},"class":"foot","title":"d","pid":4,
             "fullscreen":3}]"#;
        let monitors = r#"[
            {"id":0,"name":"DP-2","focused":true,"activeWorkspace":{"id":2,"name":"2"}},
            {"id":1,"name":"HDMI-A-1","activeWorkspace":{"id":1,"name":"1"}}]"#;
        let items = hyprland_items(clients.as_bytes(), monitors.as_bytes(), 0).unwrap();
        let badges: Vec<_> = items.iter().map(|item| item.badge.as_deref()).collect();
        assert_eq!(
            badges,
            [
                Some("Pinned"),
                Some("Maximized"),
                Some("Fullscreen"),
                Some("Fullscreen")
            ]
        );
        assert_eq!(items[0].subtitle, "mpv on workspace 1 (HDMI-A-1)");
        assert!(items[0]
            .details
            .contains(&("State".into(), "Pinned, Floating, XWayland".into())));
        assert!(items[1]
            .details
            .contains(&("State".into(), "Maximized, Grouped".into())));
        assert_eq!(items[2].subtitle, "foot on workspace 2");
        let close = items[0].close.as_ref().unwrap();
        assert!(close.keep_open && close.confirm.is_none());
        assert!(matches!(
            &close.action,
            DeferredAction::Hypr(HyprAction::Close { address }) if address == "0x1"
        ));

        let one =
            r#"[{"id":0,"name":"DP-2","focused":true,"activeWorkspace":{"id":2,"name":"2"}}]"#;
        let items = hyprland_items(clients.as_bytes(), one.as_bytes(), 0).unwrap();
        assert_eq!(items[0].subtitle, "mpv on workspace 1");
    }

    #[test]
    fn test_pick_process_window_matches_title_then_single_window() {
        let clients = r#"[
            {"address":"0x1","workspace":{"id":1,"name":"1"},"title":"Mail — Mozilla Firefox","pid":100},
            {"address":"0x2","workspace":{"id":2,"name":"2"},"title":"Raffi docs — Mozilla Firefox","pid":100},
            {"address":"0x3","workspace":{"id":2,"name":"2"},"title":"Raffi docs - Google Chrome","pid":200},
            {"address":"0x4","workspace":{"id":2,"name":"2"},"title":"gone","pid":300,"mapped":false}]"#;
        let windows = process_windows(Compositor::Hyprland, clients.as_bytes()).unwrap();
        let address = |focus: Option<&DeferredAction>| match focus {
            Some(DeferredAction::Hypr(HyprAction::Focus { address })) => Some(address.clone()),
            other => panic!("unexpected focus {other:?}"),
        };
        assert_eq!(
            address(pick_process_window(&windows, 100, " Raffi docs ", false)),
            Some("0x2".into())
        );
        assert_eq!(
            address(pick_process_window(&windows, 200, "Raffi docs", false)),
            Some("0x3".into())
        );
        // The title has not followed the switch yet: wait, then fall back
        // to the only window of the process.
        assert!(pick_process_window(&windows, 200, "Other", false).is_none());
        assert_eq!(
            address(pick_process_window(&windows, 200, "Other", true)),
            Some("0x3".into())
        );
        assert!(pick_process_window(&windows, 100, "Other", true).is_none());
        assert!(pick_process_window(&windows, 300, "", true).is_none());
    }

    #[test]
    fn test_pick_app_window_matches_class_and_reverse_dns_id() {
        let clients = r#"[
            {"address":"0x1","workspace":{"id":1,"name":"1"},"class":"slack-term","pid":1},
            {"address":"0x2","workspace":{"id":7,"name":"7"},"class":"","initialClass":"Slack","pid":2},
            {"address":"0x3","workspace":{"id":8,"name":"8"},"class":"com.slack.Slack","pid":3}]"#;
        let windows = process_windows(Compositor::Hyprland, clients.as_bytes()).unwrap();
        assert!(matches!(
            pick_app_window(&windows, &["slack"]),
            Some(DeferredAction::Hypr(HyprAction::Focus { address })) if address == "0x2"
        ));
        assert!(matches!(
            pick_app_window(&windows[2..], &["slack"]),
            Some(DeferredAction::Hypr(HyprAction::Focus { address })) if address == "0x3"
        ));

        // The most recently focused window of any of the names wins.
        let browsers = r#"[
            {"address":"0x4","workspace":{"id":1,"name":"1"},"class":"google-chrome","pid":4,"focusHistoryID":3},
            {"address":"0x5","workspace":{"id":2,"name":"2"},"class":"firefox","pid":5,"focusHistoryID":1},
            {"address":"0x6","workspace":{"id":3,"name":"3"},"class":"google-chrome","pid":6,"focusHistoryID":2},
            {"address":"0x7","workspace":{"id":4,"name":"4"},"class":"google-chrome","pid":7,"focusHistoryID":-1}]"#;
        let windows = process_windows(Compositor::Hyprland, browsers.as_bytes()).unwrap();
        assert!(matches!(
            pick_app_window(&windows, &["google-chrome", "google-chrome-stable"]),
            Some(DeferredAction::Hypr(HyprAction::Focus { address })) if address == "0x6"
        ));
    }

    #[test]
    fn test_process_windows_niri_and_sway() {
        let niri_windows = process_windows(Compositor::Niri, NIRI_WINDOWS.as_bytes()).unwrap();
        let focus = pick_process_window(&niri_windows, 1767294, "term two", false).unwrap();
        assert!(
            matches!(focus, DeferredAction::Command(argv) if argv == &niri("focus-window --id 3"))
        );

        let sway_windows = process_windows(Compositor::Sway, SWAY_TREE.as_bytes()).unwrap();
        let focus = pick_process_window(&sway_windows, 1846750, "term B", false).unwrap();
        assert!(
            matches!(focus, DeferredAction::Command(argv) if argv == &swaymsg("[con_id=8] focus"))
        );

        assert!(process_windows(Compositor::Sway, b"nope").is_err());

        let focus = pick_app_window(&niri_windows, &["KITTY"]).unwrap();
        assert!(
            matches!(focus, DeferredAction::Command(argv) if argv.contains(&"--id".to_string()))
        );
        assert!(pick_app_window(&sway_windows, &["no-such-app"]).is_none());
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

    fn workspace_rows(items: &[CommandItem]) -> Vec<(&str, &str, Option<&str>)> {
        items
            .iter()
            .map(|item| {
                (
                    item.title.as_str(),
                    item.subtitle.as_str(),
                    item.badge.as_deref(),
                )
            })
            .collect()
    }

    #[test]
    fn test_hyprland_workspace_rows() {
        let workspaces = r#"[
            {"id": 7, "name": "7", "monitor": "HDMI-A-1"},
            {"id": -98, "name": "special:magic", "monitor": "DP-2"},
            {"id": 3, "name": "3", "monitor": "DP-2"},
            {"id": -1337, "name": "web", "monitor": "HDMI-A-1"},
            {"id": 2, "name": "2", "monitor": "DP-2"},
            {"id": 1, "name": "1", "monitor": "DP-2"}
        ]"#;
        let items = hyprland_workspace_items(
            workspaces.as_bytes(),
            ACTIVE.as_bytes(),
            CLIENTS.as_bytes(),
            4242,
        )
        .unwrap();
        assert_eq!(
            workspace_rows(&items),
            vec![
                ("Workspace 1", "on DP-2 · No windows", Some("Current")),
                ("Workspace 2", "on DP-2 · 1 window · tab", None),
                ("Workspace 3", "on DP-2 · 1 window · Mozilla Firefox", None),
                ("Workspace 7", "on HDMI-A-1 · No windows", Some("Visible")),
                ("Workspace web", "on HDMI-A-1 · No windows", None),
                (
                    "Workspace special:magic",
                    "on DP-2 · 1 window · notes",
                    None
                ),
            ]
        );
        assert!(items[4].dimmed && !items[0].dimmed);

        let action = |item: &CommandItem| item.primary.as_ref().unwrap().action.clone();
        assert!(matches!(
            action(&items[4]),
            DeferredAction::Hypr(HyprAction::Workspace { workspace }) if workspace == "name:web"
        ));
        assert!(matches!(
            action(&items[5]),
            DeferredAction::Hypr(HyprAction::ToggleSpecial { name }) if name == "magic"
        ));
        // The last focused window, notes, moves anywhere but its own workspace.
        assert!(matches!(
            &items[0].secondary.as_ref().unwrap().action,
            DeferredAction::Hypr(HyprAction::MoveTo { address, workspace })
                if address == "0x3c" && workspace == "1"
        ));
        assert!(items[5].secondary.is_none());
    }

    #[test]
    fn test_hyprland_workspace_special_shown_is_current() {
        let workspaces = r#"[
            {"id": 1, "name": "1", "monitor": "DP-2"},
            {"id": -98, "name": "special:magic", "monitor": "DP-2"}
        ]"#;
        let monitors = r#"[{"name": "DP-2", "focused": true,
            "activeWorkspace": {"id": 1, "name": "1"},
            "specialWorkspace": {"id": -98, "name": "special:magic"}}]"#;
        let items =
            hyprland_workspace_items(workspaces.as_bytes(), monitors.as_bytes(), b"[]", 4242)
                .unwrap();
        assert_eq!(
            workspace_rows(&items),
            vec![
                ("Workspace 1", "No windows", Some("Visible")),
                ("Workspace special:magic", "No windows", Some("Current")),
            ]
        );
        assert!(items.iter().all(|item| item.secondary.is_none()));
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
        assert_eq!(
            detect(&[("SWAYSOCK", "/run/sway.sock")]),
            Some(Compositor::Sway)
        );
        assert_eq!(
            detect(&[
                ("SWAYSOCK", "/run/sway.sock"),
                ("NIRI_SOCKET", "/run/niri.sock")
            ]),
            Some(Compositor::Niri)
        );
        assert_eq!(
            detect(&[
                ("SWAYSOCK", "/run/sway.sock"),
                ("HYPRLAND_INSTANCE_SIGNATURE", "abc")
            ]),
            Some(Compositor::Hyprland)
        );
        assert_eq!(detect(&[("SWAYSOCK", "")]), None);
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
        assert_eq!(items[0].badge, None);
        assert_eq!(items[1].badge.as_deref(), Some("Floating"));
        assert_eq!(
            argvs(items[1].close.as_ref().unwrap()),
            vec![niri("close-window --id 3")]
        );

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
    fn test_niri_workspace_rows() {
        let items = niri_workspace_items(NIRI_WINDOWS.as_bytes(), NIRI_WORKSPACES.as_bytes(), 4242)
            .unwrap();
        assert_eq!(
            workspace_rows(&items),
            vec![
                ("Workspace web", "1 window · term two", None),
                ("Workspace 2", "1 window · term one", Some("Current")),
                ("Workspace 3", "No windows", None),
            ]
        );
        assert_eq!(
            argvs(items[0].primary.as_ref().unwrap()),
            vec![niri("focus-workspace web")]
        );
        assert!(items[1].secondary.is_none());
        assert_eq!(
            argvs(items[2].secondary.as_ref().unwrap()),
            vec![
                niri("move-window-to-workspace --window-id 2 --focus false -- 3"),
                niri("focus-window --id 2"),
            ]
        );
    }

    #[test]
    fn test_niri_workspace_rows_skip_nameless_windows() {
        // A window with neither a title nor an app id is not a row in the
        // Windows view, so the workspace rows skip it too.
        let windows = r#"[
            {"id":3,"title":"term two","app_id":"kitty","pid":1767294,"workspace_id":1,
             "is_focused":false,"is_floating":false},
            {"id":9,"title":null,"app_id":null,"pid":1767295,"workspace_id":1,
             "is_focused":false,"is_floating":false},
            {"id":10,"title":"","app_id":"","pid":1767296,"workspace_id":3,
             "is_focused":false,"is_floating":false}
        ]"#;
        let items =
            niri_workspace_items(windows.as_bytes(), NIRI_WORKSPACES.as_bytes(), 4242).unwrap();
        assert_eq!(
            workspace_rows(&items),
            vec![
                ("Workspace web", "1 window · term two", None),
                ("Workspace 2", "No windows", Some("Current")),
                ("Workspace 3", "No windows", None),
            ]
        );
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
        let items = Compositor::Niri.load_windows(false).unwrap();
        let item = items.iter().find(|item| item.identity == id).unwrap();
        let run = |action: &CommandAction| {
            super::super::actions::run_deferred(&std::sync::Mutex::new(Some(action.action.clone())))
        };
        run(item.secondary.as_ref().unwrap()).unwrap();
        let items = Compositor::Niri.load_windows(false).unwrap();
        assert_eq!(items[0].identity, id, "the moved window has the focus");
        run(items[1].primary.as_ref().unwrap()).unwrap();
        let items = Compositor::Niri.load_windows(false).unwrap();
        assert_ne!(items[0].identity, id, "the other window has the focus");
    }

    #[test]
    fn test_niri_quit_result() {
        use std::os::unix::process::ExitStatusExt;
        let failed = ExitStatus::from_raw(1 << 8);
        // Captured from niri 26.04 after a successful quit.
        let quit = "Error: error communicating with niri\n\nCaused by:\n    EOF while parsing a value at line 1 column 0\n";
        assert!(quit_result("niri", failed, quit, niri_closed).is_ok());
        assert!(quit_result("niri", ExitStatus::from_raw(0), "", niri_closed).is_ok());
        assert_eq!(
            quit_result(
                "niri",
                failed,
                "Error: error connecting to the niri socket\n\nCaused by:\n    No such file or directory (os error 2)\n",
                niri_closed
            )
            .unwrap_err(),
            "niri could not quit: error connecting to the niri socket"
        );
        assert_eq!(
            quit_result(
                "niri",
                failed,
                "Error: error communicating with niri\n\nCaused by:\n    expected value at line 1 column 1\n",
                niri_closed
            )
            .unwrap_err(),
            "niri could not quit: error communicating with niri"
        );
    }

    #[test]
    fn test_sway_quit_result() {
        use std::os::unix::process::ExitStatusExt;
        let failed = ExitStatus::from_raw(1 << 8);
        // Captured from sway 1.12 after a successful exit.
        let exited =
            "00:00:00.003 [ERROR] [common/ipc-client.c:86] Unable to receive IPC response\n";
        assert!(quit_result("sway", failed, exited, sway_closed).is_ok());
        assert_eq!(
            quit_result(
                "sway",
                failed,
                "00:00:00.000 [ERROR] [common/ipc-client.c:71] Unable to connect to /run/user/1000/sway.sock\n",
                sway_closed
            )
            .unwrap_err(),
            "sway could not quit: Unable to connect to /run/user/1000/sway.sock"
        );
        assert!(quit_result("sway", failed, exited, niri_closed).is_err());
    }

    // Captured from a headless sway 1.12 with two outputs, trimmed to the
    // fields Raffi reads. Focus went to term D, term B, x kitty (Xwayland),
    // then term A; scratch one sits in the scratchpad.
    const SWAY_TREE: &str = r#"
    {"id":1,"type":"root","name":"root","focus":[3,5],
     "nodes":[
      {"id":2147483647,"type":"output","name":"__i3","focus":[2147483646],
       "nodes":[
        {"id":2147483646,"type":"workspace","name":"__i3_scratch","focus":[14],
         "nodes":[],
         "floating_nodes":[
          {"id":14,"type":"floating_con","name":"scratch one","pid":1848336,"app_id":"scratchy","shell":"xdg_shell","focus":[],
           "nodes":[],"floating_nodes":[]}
         ]}
       ],
       "floating_nodes":[]},
      {"id":3,"type":"output","name":"HEADLESS-1","focus":[4,11],
       "nodes":[
        {"id":4,"type":"workspace","name":"1","focus":[7,9],
         "nodes":[
          {"id":7,"type":"con","name":"term A","pid":1846505,"app_id":"termA","shell":"xdg_shell","focus":[],
           "nodes":[],"floating_nodes":[]},
          {"id":9,"type":"con","name":null,"focus":[8,10],
           "nodes":[
            {"id":8,"type":"con","name":"term B","pid":1846750,"app_id":"termB","shell":"xdg_shell","focus":[],
             "nodes":[],"floating_nodes":[]},
            {"id":10,"type":"con","name":"term C","pid":1847043,"app_id":"termC","shell":"xdg_shell","focus":[],
             "nodes":[],"floating_nodes":[]}
           ],
           "floating_nodes":[]}
         ],
         "floating_nodes":[]},
        {"id":11,"type":"workspace","name":"web","focus":[12,13],
         "nodes":[
          {"id":12,"type":"con","name":"x kitty","pid":1847415,"app_id":null,"shell":"xwayland","window_properties":{"class":"XKitty","instance":"XKitty"},"focus":[],
           "nodes":[],"floating_nodes":[]}
         ],
         "floating_nodes":[
          {"id":13,"type":"floating_con","name":"floating one","pid":1848019,"app_id":"floaty","shell":"xdg_shell","focus":[],
           "nodes":[],"floating_nodes":[]}
         ]}
       ],
       "floating_nodes":[]},
      {"id":5,"type":"output","name":"HEADLESS-2","focus":[15],
       "nodes":[
        {"id":15,"type":"workspace","name":"3","focus":[16],
         "nodes":[
          {"id":16,"type":"con","name":"term D","pid":1848572,"app_id":"termD","shell":"xdg_shell","focus":[],
           "nodes":[],"floating_nodes":[]}
         ],
         "floating_nodes":[]}
       ],
       "floating_nodes":[]}
     ],
     "floating_nodes":[]}"#;
    const SWAY_WORKSPACES: &str = r#"[
        {"id":4,"num":1,"name":"1","visible":true,"focused":true,"output":"HEADLESS-1"},
        {"id":11,"num":-1,"name":"web","visible":false,"focused":false,"output":"HEADLESS-1"},
        {"id":15,"num":3,"name":"3","visible":true,"focused":false,"output":"HEADLESS-2"}
    ]"#;

    fn swaymsg(command: &str) -> Vec<String> {
        vec!["swaymsg".to_string(), command.to_string()]
    }

    #[test]
    fn test_sway_items_order_and_rows() {
        let items = sway_items(SWAY_TREE.as_bytes(), SWAY_WORKSPACES.as_bytes(), 1847043).unwrap();
        let ids: Vec<&str> = items.iter().map(|item| item.identity.as_str()).collect();
        assert_eq!(ids, vec!["7", "8", "12", "13", "16", "14"]);
        assert_eq!(items[0].title, "term A");
        assert_eq!(items[0].subtitle, "termA on workspace 1 (HEADLESS-1)");
        let xwayland = &items[2];
        assert_eq!(xwayland.subtitle, "XKitty on workspace web (HEADLESS-1)");
        assert_eq!(xwayland.icon_names, vec!["XKitty", "xkitty"]);
        assert_eq!(
            xwayland.details[0],
            ("Class".to_string(), "XKitty".to_string())
        );
        assert_eq!(items[0].details[0].0, "App ID");
        assert_eq!(items[4].subtitle, "termD on workspace 3 (HEADLESS-2)");
        assert_eq!(items[5].subtitle, "scratchy on workspace scratchpad");
        let badges: Vec<_> = items.iter().map(|item| item.badge.as_deref()).collect();
        assert_eq!(
            badges,
            [
                None,
                None,
                Some("XWayland"),
                Some("Floating"),
                None,
                Some("Floating")
            ]
        );
        assert_eq!(
            argvs(items[0].close.as_ref().unwrap()),
            vec![swaymsg("[con_id=7] kill")]
        );

        let one_output = r#"[{"name":"web","focused":true,"output":"HEADLESS-1"}]"#;
        let items = sway_items(SWAY_TREE.as_bytes(), one_output.as_bytes(), 0).unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(items[0].subtitle, "termA on workspace 1");
    }

    #[test]
    fn test_sway_workspace_rows() {
        let items = sway_workspace_items(SWAY_TREE.as_bytes(), SWAY_WORKSPACES.as_bytes(), 1847043)
            .unwrap();
        assert_eq!(
            workspace_rows(&items),
            vec![
                (
                    "Workspace 1",
                    "on HEADLESS-1 · 2 windows · term A",
                    Some("Current")
                ),
                ("Workspace web", "on HEADLESS-1 · 2 windows · x kitty", None),
                (
                    "Workspace 3",
                    "on HEADLESS-2 · 1 window · term D",
                    Some("Visible")
                ),
            ]
        );
        assert_eq!(
            argvs(items[1].primary.as_ref().unwrap()),
            vec![swaymsg("workspace --no-auto-back-and-forth \"web\"")]
        );
        assert!(items[0].secondary.is_none());
        assert_eq!(
            argvs(items[2].secondary.as_ref().unwrap()),
            vec![
                swaymsg("[con_id=7] move --no-auto-back-and-forth container to workspace \"3\""),
                swaymsg("[con_id=7] focus"),
            ]
        );
    }

    #[test]
    fn test_sway_items_skip_containers_and_unnamed_windows() {
        let tree = r#"{"id":1,"type":"root","name":"root","focus":[4],"nodes":[
            {"id":4,"type":"workspace","name":"2","focus":[6,5,7],"nodes":[
                {"id":5,"type":"con","name":"no app","pid":10,"app_id":null,"nodes":[]},
                {"id":6,"type":"con","name":null,"pid":11,"app_id":"foot"},
                {"id":7,"type":"con","name":"split","nodes":[]},
                {"id":8,"type":"con","name":"","pid":12,"app_id":""}]}]}"#;
        let items = sway_items(tree.as_bytes(), b"[]", 0).unwrap();
        let titles: Vec<&str> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(titles, ["foot", "no app"]);
        assert_eq!(items[0].subtitle, "foot on workspace 2");
        assert_eq!(items[1].subtitle, "Workspace 2");
        assert!(items[0].secondary.is_none(), "no focused workspace");
    }

    #[test]
    fn test_sway_actions() {
        let items = sway_items(SWAY_TREE.as_bytes(), SWAY_WORKSPACES.as_bytes(), 0).unwrap();
        let xwayland = items.iter().find(|item| item.identity == "12").unwrap();
        assert_eq!(
            argvs(xwayland.primary.as_ref().unwrap()),
            vec![swaymsg("[con_id=12] focus")]
        );
        assert_eq!(
            argvs(xwayland.secondary.as_ref().unwrap()),
            vec![
                swaymsg(r#"[con_id=12] move --no-auto-back-and-forth container to workspace "1""#),
                swaymsg("[con_id=12] focus"),
            ]
        );

        let hostile = r#"[{"name":"a\";exec x, b","focused":true,"output":"HEADLESS-1"}]"#;
        let items = sway_items(SWAY_TREE.as_bytes(), hostile.as_bytes(), 0).unwrap();
        assert_eq!(
            argvs(items[0].secondary.as_ref().unwrap())[0],
            swaymsg(
                r#"[con_id=7] move --no-auto-back-and-forth container to workspace "a"'"'";exec x, b""#
            )
        );
        let keyword = r#"[{"name":"Current","focused":true,"output":"HEADLESS-1"}]"#;
        let items = sway_items(SWAY_TREE.as_bytes(), keyword.as_bytes(), 0).unwrap();
        assert!(items[0].secondary.is_none());
    }

    #[test]
    fn test_sway_quote() {
        // Each quoted name was checked against sway 1.12 by creating the
        // workspace and moving a window to it.
        for (name, quoted) in [
            ("1", r#""1""#),
            (r#"a"b"#, r#""a"'"'"b""#),
            (r#""lead"#, r#"""'"'"lead""#),
            ("x;y,z", r#""x;y,z""#),
            ("with space", r#""with space""#),
            ("number 5", r#""number 5""#),
            ("it's", r#""it's""#),
            ("[x]", r#""[x]""#),
            (r"back\slash", r#""back\slash""#),
            (r#"a\"b"#, r#""a\"b""#),
            (r"q\\", r#""q\\""#),
            ("$mod", r#""$$mod""#),
            ("a$$b", r#""a$$$$b""#),
            (r"a\$b", r#""a\$b""#),
            (r"a\\$b", r#""a\\$$b""#),
        ] {
            assert_eq!(sway_quote(name).as_deref(), Some(quoted), "{name}");
        }
        for name in ["", r"q\", r"a\\\", "next", "Back_And_Forth", "number"] {
            assert_eq!(sway_quote(name), None, "{name}");
        }
    }

    #[test]
    fn test_sway_items_reject_invalid_json() {
        assert_eq!(
            sway_items(b"[]", SWAY_WORKSPACES.as_bytes(), 1).unwrap_err(),
            "sway returned invalid window data"
        );
        assert_eq!(
            sway_items(SWAY_TREE.as_bytes(), b"{}", 1).unwrap_err(),
            "sway returned invalid workspace data"
        );
    }

    /// Focuses and moves a window in a sway session; never run by default.
    /// Point SWAYSOCK at a throwaway headless sway with two windows, the
    /// focused one on another workspace than `RAFFI_SWAY_TEST_ID`:
    /// `RAFFI_SWAY_TEST_ID=12 cargo test --all-features -- --ignored live_sway`
    #[test]
    #[ignore]
    fn live_sway_actions() {
        let id = std::env::var("RAFFI_SWAY_TEST_ID").unwrap();
        let items = Compositor::Sway.load_windows(false).unwrap();
        let item = items.iter().find(|item| item.identity == id).unwrap();
        let run = |action: &CommandAction| {
            super::super::actions::run_deferred(&std::sync::Mutex::new(Some(action.action.clone())))
        };
        run(item.secondary.as_ref().unwrap()).unwrap();
        let items = Compositor::Sway.load_windows(false).unwrap();
        assert_eq!(items[0].identity, id, "the moved window has the focus");
        run(items[1].primary.as_ref().unwrap()).unwrap();
        let items = Compositor::Sway.load_windows(false).unwrap();
        assert_ne!(items[0].identity, id, "the other window has the focus");
    }
}
