use super::actions::DeferredAction;
use super::command_list::{CommandAction, CommandItem, QUERY_ROW};
use crate::terminal::{TabKind, TabTarget};

pub(super) const TERMINAL_ICON: &str = "utilities-terminal";

/// The terminal's program name, such as `kitty` for `/usr/bin/kitty -e`.
pub(super) fn terminal_name(prefix: &[String]) -> &str {
    prefix
        .first()
        .map(|program| program.rsplit('/').next().unwrap_or(program))
        .unwrap_or_default()
}

/// What runs `command`: a new tab of `tab` when there is one, falling back
/// to a new window.
pub(super) fn run_action(
    prefix: &[String],
    shell: &str,
    command: &str,
    hold: bool,
    tab: Option<&TabTarget>,
) -> DeferredAction {
    let window = crate::terminal::shell_argv(prefix, shell, command, hold);
    let Some(target) = tab else {
        return DeferredAction::Command(window);
    };
    let cwd = std::env::current_dir().ok();
    let cwd = cwd.as_deref().and_then(std::path::Path::to_str);
    // kitty titles the tab after the command, which then names its window.
    // WezTerm's window title follows its own format, so its window is only
    // found when it has a single one.
    let title = match target.kind {
        TabKind::Kitty => command,
        TabKind::WezTerm => "",
    };
    DeferredAction::TerminalTab {
        kind: target.kind,
        tab: crate::terminal::tab_argv(target, prefix, shell, command, hold, cwd),
        window,
        raise: target.pid.map(|pid| (pid, title.to_string())),
    }
}

/// The row running `query` in the terminal, or a hint while it is empty.
#[allow(dead_code)]
pub(super) fn run_item(
    query: &str,
    prefix: &[String],
    shell: &str,
    hold: bool,
    tab: Option<&TabTarget>,
) -> CommandItem {
    run_item_with(query, prefix, shell, hold, tab, false)
}

pub(super) fn run_item_with(
    query: &str,
    prefix: &[String],
    shell: &str,
    hold: bool,
    tab: Option<&TabTarget>,
    keep_open: bool,
) -> CommandItem {
    let name = terminal_name(prefix);
    let command = query.trim();
    if command.is_empty() {
        return CommandItem::info(
            QUERY_ROW,
            format!("Type a command to run in {name}"),
            TERMINAL_ICON,
        );
    }
    let (subtitle, opens_in) = match tab {
        Some(target) => {
            let kind = target.kind.name();
            (format!("in a new {kind} tab"), format!("New {kind} tab"))
        }
        None => (format!("in {name}"), "New window".to_string()),
    };
    CommandItem {
        title: format!("Run {command}"),
        subtitle,
        match_text: String::new(),
        icon_names: vec![TERMINAL_ICON.to_string()],
        icon: None,
        identity: QUERY_ROW.to_string(),
        unavailable: None,
        primary: Some(CommandAction {
            action: run_action(prefix, shell, command, hold, tab),
            confirm: None,
            keep_open,
            progress: None,
        }),
        secondary: None,
        details: vec![
            ("Command".to_string(), command.to_string()),
            ("Terminal".to_string(), prefix.join(" ")),
            ("Opens in".to_string(), opens_in),
            ("Shell".to_string(), shell.to_string()),
        ],
        badge: None,
        domains: Vec::new(),
        dimmed: false,
        sort_priority: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_item() {
        let prefix = vec!["/usr/bin/foot".to_string()];
        let hint = run_item("  ", &prefix, "bash", true, None);
        assert_eq!(hint.title, "Type a command to run in foot");
        assert!(hint.primary.is_none());

        let item = run_item(" echo 'a b' ", &prefix, "zsh", false, None);
        assert_eq!(item.title, "Run echo 'a b'");
        assert_eq!(item.subtitle, "in foot");
        let Some(DeferredAction::Command(argv)) = item.primary.map(|action| action.action) else {
            panic!("command action");
        };
        assert_eq!(
            argv,
            [
                "/usr/bin/foot",
                "zsh",
                "-c",
                "eval \"$1\"",
                "raffi",
                "echo 'a b'"
            ]
        );
    }
}
