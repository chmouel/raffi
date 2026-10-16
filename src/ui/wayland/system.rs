use super::actions::DeferredAction;
use super::command_list::{CommandAction, CommandItem};
use crate::{SystemAddonConfig, SystemCommand};

/// One row of the system commands list.
#[derive(Debug, Clone)]
pub(super) struct SystemEntry {
    pub id: SystemCommand,
    pub title: &'static str,
    /// Completes "Press Enter again to ..." on entries needing confirmation.
    pub verb: &'static str,
    pub icon: &'static str,
    pub action: DeferredAction,
    /// The program that runs the action, when it is not installed.
    pub missing: Option<String>,
}

impl SystemEntry {
    pub fn needs_confirmation(&self) -> bool {
        self.id != SystemCommand::Lock
    }

    /// The command shown under the title.
    pub fn command_line(&self) -> String {
        match &self.action {
            DeferredAction::Command(argv) => argv.join(" "),
            DeferredAction::HyprDispatch(args) => format!("hyprctl dispatch {}", args.join(" ")),
            DeferredAction::HyprPaste(_)
            | DeferredAction::HyprDispatchBatch(_)
            | DeferredAction::Signal(_)
            | DeferredAction::Bluetooth(_) => String::new(),
        }
    }
}

const ENTRIES: [(SystemCommand, &str, &str, &str); 6] = [
    (SystemCommand::Lock, "Lock", "lock", "system-lock-screen"),
    (
        SystemCommand::Suspend,
        "Suspend",
        "suspend",
        "system-suspend",
    ),
    (
        SystemCommand::Hibernate,
        "Hibernate",
        "hibernate",
        "system-hibernate",
    ),
    (
        SystemCommand::Logout,
        "Log out",
        "log out",
        "system-log-out",
    ),
    (SystemCommand::Reboot, "Reboot", "reboot", "system-reboot"),
    (
        SystemCommand::Shutdown,
        "Shut down",
        "shut down",
        "system-shutdown",
    ),
];

fn default_action(id: SystemCommand, hyprland: bool, session_id: Option<&str>) -> DeferredAction {
    let argv =
        |args: &[&str]| DeferredAction::Command(args.iter().map(|s| s.to_string()).collect());
    match id {
        SystemCommand::Lock => argv(&["loginctl", "lock-session"]),
        SystemCommand::Suspend => argv(&["systemctl", "suspend"]),
        SystemCommand::Hibernate => argv(&["systemctl", "hibernate"]),
        SystemCommand::Logout if hyprland => DeferredAction::HyprDispatch(vec!["exit".into()]),
        SystemCommand::Logout => {
            let mut args = vec!["loginctl", "terminate-session"];
            args.extend(session_id);
            argv(&args)
        }
        SystemCommand::Reboot => argv(&["systemctl", "reboot"]),
        SystemCommand::Shutdown => argv(&["systemctl", "poweroff"]),
    }
}

fn program(action: &DeferredAction) -> Option<&str> {
    match action {
        DeferredAction::Command(argv) => argv.first().map(String::as_str),
        DeferredAction::HyprDispatch(_) | DeferredAction::HyprDispatchBatch(_) => Some("hyprctl"),
        DeferredAction::HyprPaste(_) | DeferredAction::Signal(_) => None,
        DeferredAction::Bluetooth(_) => Some("bluetoothctl"),
    }
}

/// The visible entries for `config`. `hyprland` and `session_id` pick the
/// default log out command; `exists` reports whether a program is installed.
pub(super) fn system_entries(
    config: &SystemAddonConfig,
    hyprland: bool,
    session_id: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Vec<SystemEntry> {
    ENTRIES
        .iter()
        .filter(|(id, ..)| !config.hide.contains(id))
        .filter_map(|&(id, title, verb, icon)| {
            let (action, overridden) = match config.commands.get(id) {
                Some(argv) if argv.is_empty() => return None,
                Some(argv) => (DeferredAction::Command(argv.clone()), true),
                None => (default_action(id, hyprland, session_id), false),
            };
            let no_session =
                id == SystemCommand::Logout && !overridden && !hyprland && session_id.is_none();
            let missing = if no_session {
                Some("XDG_SESSION_ID".to_string())
            } else {
                program(&action)
                    .filter(|program| !exists(program))
                    .map(str::to_string)
            };
            Some(SystemEntry {
                id,
                title,
                verb,
                icon,
                action,
                missing,
            })
        })
        .collect()
}

impl SystemEntry {
    pub fn to_item(&self) -> CommandItem {
        let enter = if self.needs_confirmation() {
            format!("Press twice to {}", self.verb)
        } else {
            format!("{} the session", self.title)
        };
        CommandItem {
            title: self.title.to_string(),
            subtitle: self.command_line(),
            match_text: self.title.to_string(),
            icon_names: vec![self.icon.to_string()],
            icon: None,
            identity: format!("{:?}", self.id).to_lowercase(),
            unavailable: self
                .missing
                .as_ref()
                .map(|program| format!("{program} not found")),
            primary: Some(CommandAction {
                action: self.action.clone(),
                confirm: self.needs_confirmation().then(|| self.verb.to_string()),
                keep_open: false,
                progress: None,
            }),
            secondary: None,
            details: vec![
                ("Command".to_string(), self.command_line()),
                ("Enter".to_string(), enter),
            ],
        }
    }
}
