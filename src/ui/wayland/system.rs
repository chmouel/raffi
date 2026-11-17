use std::borrow::Cow;

use super::actions::DeferredAction;
use super::command_list::{CommandAction, CommandItem};
use super::hyprland::HyprAction;
use super::windows::Compositor;
use crate::{SystemAddonConfig, SystemCommand};

/// Identity of a row in the system commands list: one of the seven built-in
/// commands, or a user-configured `addons.system.extra` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SystemEntryId {
    Builtin(SystemCommand),
    Extra(String),
}

impl SystemEntryId {
    #[cfg(test)]
    pub fn as_builtin(&self) -> Option<SystemCommand> {
        match self {
            Self::Builtin(id) => Some(*id),
            Self::Extra(_) => None,
        }
    }

    pub fn identity(&self) -> String {
        match self {
            Self::Builtin(id) => format!("{id:?}").to_lowercase(),
            Self::Extra(id) => id.clone(),
        }
    }
}

impl PartialEq<SystemCommand> for SystemEntryId {
    fn eq(&self, other: &SystemCommand) -> bool {
        matches!(self, Self::Builtin(id) if id == other)
    }
}

/// One row of the system commands list.
#[derive(Debug, Clone)]
pub(super) struct SystemEntry {
    pub id: SystemEntryId,
    pub title: Cow<'static, str>,
    /// Completes "Press Enter again to ..." on entries needing confirmation.
    pub verb: Cow<'static, str>,
    pub icon: Cow<'static, str>,
    pub action: DeferredAction,
    /// The program that runs the action, when it is not installed.
    pub missing: Option<String>,
    pub confirm: bool,
    pub keep_open: bool,
}

impl SystemEntry {
    pub fn needs_confirmation(&self) -> bool {
        self.confirm
    }

    /// The command shown under the title.
    pub fn command_line(&self) -> String {
        match &self.action {
            DeferredAction::Command(argv) => argv.join(" "),
            DeferredAction::CommandBatch(commands) => commands
                .iter()
                .map(|argv| argv.join(" "))
                .collect::<Vec<_>>()
                .join("; "),
            // `hyprctl eval` with a Lua config, `hyprctl dispatch exit` otherwise.
            DeferredAction::Hypr(HyprAction::Exit) => {
                "Hyprland exit dispatcher via hyprctl".to_string()
            }
            // `hyprctl eval` with a Lua config, `hyprctl dispatch dpms off` otherwise.
            DeferredAction::Hypr(HyprAction::DisplayOff) => {
                "Hyprland dpms off dispatcher via hyprctl".to_string()
            }
            DeferredAction::NiriQuit => super::windows::NIRI_QUIT.join(" "),
            DeferredAction::SwayExit => super::windows::SWAY_EXIT.join(" "),
            DeferredAction::Hypr(_)
            | DeferredAction::HyprPaste(_)
            | DeferredAction::Signal(_)
            | DeferredAction::Bluetooth(_)
            | DeferredAction::Secret(_)
            | DeferredAction::BrowserTab(_)
            | DeferredAction::BrowserHostSetup { .. }
            | DeferredAction::TerminalTab { .. }
            | DeferredAction::Slack(_)
            | DeferredAction::Copy(_)
            | DeferredAction::OpenInApp { .. } => String::new(),
        }
    }
}

const ENTRIES: [(SystemCommand, &str, &str, &str); 7] = [
    (SystemCommand::Lock, "Lock", "lock", "system-lock-screen"),
    (
        SystemCommand::DisplayOff,
        "Display off",
        "turn the display off",
        "video-display",
    ),
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

fn default_action(
    id: SystemCommand,
    compositor: Option<Compositor>,
    session_id: Option<&str>,
) -> Option<DeferredAction> {
    let argv =
        |args: &[&str]| DeferredAction::Command(args.iter().map(|s| s.to_string()).collect());
    match id {
        SystemCommand::Lock => Some(argv(&["loginctl", "lock-session"])),
        SystemCommand::DisplayOff => match compositor {
            Some(Compositor::Hyprland) => Some(DeferredAction::Hypr(HyprAction::DisplayOff)),
            Some(Compositor::Niri) => Some(argv(&["niri", "msg", "action", "power-off-monitors"])),
            Some(Compositor::Sway) => Some(argv(&["swaymsg", "output * power off"])),
            None => None,
        },
        SystemCommand::Suspend => Some(argv(&["systemctl", "suspend"])),
        SystemCommand::Hibernate => Some(argv(&["systemctl", "hibernate"])),
        SystemCommand::Logout => Some(match compositor {
            Some(Compositor::Hyprland) => DeferredAction::Hypr(HyprAction::Exit),
            Some(Compositor::Niri) => DeferredAction::NiriQuit,
            Some(Compositor::Sway) => DeferredAction::SwayExit,
            None => {
                let mut args = vec!["loginctl", "terminate-session"];
                args.extend(session_id);
                argv(&args)
            }
        }),
        SystemCommand::Reboot => Some(argv(&["systemctl", "reboot"])),
        SystemCommand::Shutdown => Some(argv(&["systemctl", "poweroff"])),
    }
}

fn program(action: &DeferredAction) -> Option<&str> {
    match action {
        DeferredAction::Command(argv) => argv.first().map(String::as_str),
        DeferredAction::CommandBatch(commands) => commands.first()?.first().map(String::as_str),
        DeferredAction::Hypr(_) => Some("hyprctl"),
        DeferredAction::NiriQuit => Some("niri"),
        DeferredAction::SwayExit => Some("swaymsg"),
        DeferredAction::HyprPaste(_)
        | DeferredAction::Signal(_)
        | DeferredAction::Secret(_)
        | DeferredAction::BrowserTab(_)
        | DeferredAction::BrowserHostSetup { .. }
        | DeferredAction::TerminalTab { .. }
        | DeferredAction::Slack(_)
        | DeferredAction::Copy(_)
        | DeferredAction::OpenInApp { .. } => None,
        DeferredAction::Bluetooth(_) => Some("bluetoothctl"),
    }
}

fn slugify_title(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "cmd".to_string()
    } else {
        slug
    }
}

/// The visible entries for `config`. `compositor` and `session_id` select
/// context-specific defaults; `exists` reports whether a program is installed.
pub(super) fn system_entries(
    config: &SystemAddonConfig,
    compositor: Option<Compositor>,
    session_id: Option<&str>,
    exists: impl Fn(&str) -> bool,
) -> Vec<SystemEntry> {
    let mut entries: Vec<SystemEntry> = ENTRIES
        .iter()
        .filter(|(id, ..)| !config.hide.contains(id))
        .filter_map(|&(id, title, verb, icon)| {
            let (action, overridden) = match config.commands.get(id) {
                Some(argv) if argv.is_empty() => return None,
                Some(argv) => (DeferredAction::Command(argv.clone()), true),
                None => (default_action(id, compositor, session_id)?, false),
            };
            let no_session = id == SystemCommand::Logout
                && !overridden
                && compositor.is_none()
                && session_id.is_none();
            let missing = if no_session {
                Some("XDG_SESSION_ID".to_string())
            } else {
                program(&action)
                    .filter(|program| !exists(program))
                    .map(str::to_string)
            };
            let keep_open = matches!(
                id,
                SystemCommand::Lock | SystemCommand::Suspend | SystemCommand::Hibernate
            ) && matches!(
                &action,
                DeferredAction::Command(_)
                    | DeferredAction::CommandBatch(_)
                    | DeferredAction::Hypr(_)
            );
            Some(SystemEntry {
                id: SystemEntryId::Builtin(id),
                title: Cow::Borrowed(title),
                verb: Cow::Borrowed(verb),
                icon: Cow::Borrowed(icon),
                action,
                missing,
                confirm: id != SystemCommand::Lock,
                keep_open,
            })
        })
        .collect();

    for (index, extra) in config.extra.iter().enumerate() {
        let title = extra.title.trim();
        if title.is_empty() || extra.argv.is_empty() || extra.argv[0].trim().is_empty() {
            continue;
        }
        let action = DeferredAction::Command(extra.argv.clone());
        let missing = program(&action)
            .filter(|program| !exists(program))
            .map(str::to_string);
        let verb = extra
            .verb
            .as_deref()
            .map(str::trim)
            .filter(|verb| !verb.is_empty())
            .map(|verb| verb.to_string())
            .unwrap_or_else(|| title.to_lowercase());
        let icon = extra
            .icon
            .as_deref()
            .map(str::trim)
            .filter(|icon| !icon.is_empty())
            .unwrap_or("system-run")
            .to_string();
        let slug = slugify_title(title);
        entries.push(SystemEntry {
            id: SystemEntryId::Extra(format!("extra:{index}-{slug}")),
            title: Cow::Owned(title.to_string()),
            verb: Cow::Owned(verb),
            icon: Cow::Owned(icon),
            action,
            missing,
            confirm: extra.confirm.unwrap_or(true),
            keep_open: extra.keep_open.unwrap_or(false),
        });
    }

    entries
}

impl SystemEntry {
    pub fn to_item_with(&self, keep_open: bool) -> CommandItem {
        let enter = if self.needs_confirmation() {
            format!("Press twice to {}", self.verb)
        } else {
            match &self.id {
                SystemEntryId::Builtin(_) => format!("{} the session", self.title),
                SystemEntryId::Extra(_) => format!("Run {}", self.title),
            }
        };
        let can_keep_open = self.keep_open
            && matches!(
                &self.action,
                DeferredAction::Command(_)
                    | DeferredAction::CommandBatch(_)
                    | DeferredAction::Hypr(_)
            );
        CommandItem {
            title: self.title.to_string(),
            subtitle: self.command_line(),
            match_text: self.title.to_string(),
            icon_names: vec![self.icon.to_string()],
            icon: None,
            identity: self.id.identity(),
            unavailable: self
                .missing
                .as_ref()
                .map(|program| format!("{program} not found")),
            primary: Some(CommandAction {
                action: self.action.clone(),
                confirm: self.needs_confirmation().then(|| self.verb.to_string()),
                keep_open: keep_open && can_keep_open,
                progress: None,
            }),
            secondary: None,
            details: vec![
                ("Command".to_string(), self.command_line()),
                ("Enter".to_string(), enter),
            ],
            badge: None,
            domains: Vec::new(),
            dimmed: false,
            sort_priority: 0,
            typed_only: false,
            close: None,
            group: None,
            pinned: false,
        }
    }
}
