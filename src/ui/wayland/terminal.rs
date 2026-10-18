use super::actions::DeferredAction;
use super::command_list::{CommandAction, CommandItem, QUERY_ROW};

pub(super) const TERMINAL_ICON: &str = "utilities-terminal";

/// The terminal's program name, such as `kitty` for `/usr/bin/kitty -e`.
pub(super) fn terminal_name(prefix: &[String]) -> &str {
    prefix
        .first()
        .map(|program| program.rsplit('/').next().unwrap_or(program))
        .unwrap_or_default()
}

/// The row running `query` in the terminal, or a hint while it is empty.
pub(super) fn run_item(query: &str, prefix: &[String], shell: &str, hold: bool) -> CommandItem {
    let name = terminal_name(prefix);
    let command = query.trim();
    if command.is_empty() {
        return CommandItem::info(
            QUERY_ROW,
            format!("Type a command to run in {name}"),
            TERMINAL_ICON,
        );
    }
    let argv = crate::terminal::shell_argv(prefix, shell, command, hold);
    CommandItem {
        title: format!("Run {command}"),
        subtitle: format!("in {name}"),
        match_text: String::new(),
        icon_names: vec![TERMINAL_ICON.to_string()],
        icon: None,
        identity: QUERY_ROW.to_string(),
        unavailable: None,
        primary: Some(CommandAction {
            action: DeferredAction::Command(argv),
            confirm: None,
            keep_open: false,
            progress: None,
        }),
        secondary: None,
        details: vec![
            ("Command".to_string(), command.to_string()),
            ("Terminal".to_string(), prefix.join(" ")),
            ("Shell".to_string(), shell.to_string()),
        ],
        badge: None,
        domains: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_item() {
        let prefix = vec!["/usr/bin/foot".to_string()];
        let hint = run_item("  ", &prefix, "bash", true);
        assert_eq!(hint.title, "Type a command to run in foot");
        assert!(hint.primary.is_none());

        let item = run_item(" echo 'a b' ", &prefix, "zsh", false);
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
