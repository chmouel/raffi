use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use super::actions::{run_bounded, DeferredAction};
use super::command_list::{CommandAction, CommandItem};
use super::hyprland::{valid_address, HyprAction};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

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

/// List the windows of the current Hyprland session, most recently focused
/// first. Blocking: run it off the UI thread.
pub(super) fn load_windows() -> Result<Vec<CommandItem>, String> {
    let hyprctl = |args: &[&str]| {
        let mut command = Command::new("hyprctl");
        command.args(args);
        run_bounded(command, TIMEOUT, MAX_OUTPUT).map_err(|error| format!("hyprctl: {error}"))
    };
    let clients = hyprctl(&["clients", "-j"])?;
    let monitors = hyprctl(&["monitors", "-j"])?;
    window_items(&clients, &monitors, std::process::id())
}

/// Build the window rows from `hyprctl clients -j` and `hyprctl monitors -j`
/// output, leaving out the windows of `own_pid`.
pub(super) fn window_items(
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
        .map(|client| window_item(client, target.as_deref()))
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

fn window_item(client: Client, target: Option<&str>) -> CommandItem {
    let title = if client.title.is_empty() {
        client.class.clone()
    } else {
        client.title.clone()
    };
    let workspace = if client.workspace.name.is_empty() {
        client.workspace.id.to_string()
    } else {
        client.workspace.name.clone()
    };
    let subtitle = if client.class.is_empty() {
        format!("Workspace {workspace}")
    } else {
        format!("{} on workspace {workspace}", client.class)
    };
    let mut icon_names: Vec<String> = Vec::new();
    for name in [
        client.class.clone(),
        client.class.to_lowercase(),
        client.initial_class.clone(),
        client.initial_class.to_lowercase(),
    ] {
        if !name.is_empty() && !icon_names.contains(&name) {
            icon_names.push(name);
        }
    }

    let mut details = vec![
        ("Class".to_string(), client.class.clone()),
        ("Workspace".to_string(), workspace),
        ("Enter".to_string(), "Focus the window".to_string()),
    ];
    let secondary = target.map(|target| {
        details.push((
            "Ctrl+Enter".to_string(),
            "Move it to the current workspace and focus it".to_string(),
        ));
        CommandAction {
            action: DeferredAction::Hypr(HyprAction::MoveHere {
                address: client.address.clone(),
                workspace: target.to_string(),
            }),
            confirm: None,
            keep_open: false,
            progress: None,
        }
    });
    CommandItem {
        match_text: format!("{title} {}", client.class),
        title,
        subtitle,
        icon_names,
        icon: None,
        identity: client.address.clone(),
        unavailable: None,
        primary: Some(CommandAction {
            action: DeferredAction::Hypr(HyprAction::Focus {
                address: client.address,
            }),
            confirm: None,
            keep_open: false,
            progress: None,
        }),
        secondary,
        details,
    }
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
        let items = window_items(CLIENTS.as_bytes(), ACTIVE.as_bytes(), 4242).unwrap();
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
        let items = window_items(CLIENTS.as_bytes(), ACTIVE.as_bytes(), 4242).unwrap();
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
        let items = window_items(CLIENTS.as_bytes(), named.as_bytes(), 4242).unwrap();
        assert!(matches!(
            &items[0].secondary.as_ref().unwrap().action,
            DeferredAction::Hypr(HyprAction::MoveHere { workspace, .. }) if workspace == "name:web"
        ));

        let odd = monitor(
            r#"{"id": -1337, "name": "a,b"}"#,
            r#"{"id": 0, "name": ""}"#,
        );
        let items = window_items(CLIENTS.as_bytes(), odd.as_bytes(), 4242).unwrap();
        assert!(items[0].secondary.is_none());
    }

    #[test]
    fn test_move_target_prefers_open_special_workspace() {
        let special = monitor(
            r#"{"id": 1, "name": "1"}"#,
            r#"{"id": -98, "name": "special:magic"}"#,
        );
        let items = window_items(CLIENTS.as_bytes(), special.as_bytes(), 4242).unwrap();
        assert!(matches!(
            &items[1].secondary.as_ref().unwrap().action,
            DeferredAction::Hypr(HyprAction::MoveHere { workspace, .. }) if workspace == "special:magic"
        ));
    }

    #[test]
    fn test_window_items_reject_invalid_json() {
        assert_eq!(
            window_items(b"nope", ACTIVE.as_bytes(), 1).unwrap_err(),
            "hyprctl returned invalid window data"
        );
        assert_eq!(
            window_items(b"[]", b"{}", 1).unwrap_err(),
            "hyprctl returned invalid workspace data"
        );
    }
}
