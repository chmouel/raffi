use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use super::actions::DeferredAction;
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
            DeferredAction::HyprPaste(_) => String::new(),
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
        DeferredAction::HyprDispatch(_) => Some("hyprctl"),
        DeferredAction::HyprPaste(_) => None,
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

pub(super) fn filter_system_into(
    entries: &[SystemEntry],
    query: &str,
    matcher: &SkimMatcherV2,
    out: &mut Vec<usize>,
) {
    out.clear();
    if query.is_empty() {
        out.extend(0..entries.len());
        return;
    }
    let mut scored: Vec<(usize, i64)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            matcher
                .fuzzy_match(entry.title, query)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    out.extend(scored.iter().map(|&(index, _)| index));
}
