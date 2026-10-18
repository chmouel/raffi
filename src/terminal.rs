//! Find the user's terminal emulator and build the command lines that run
//! something in it.

use crate::BinaryChecker;

/// Terminals tried in order when nothing is configured, with the arguments
/// that come before the command to run.
const KNOWN: &[(&str, &[&str])] = &[
    ("kitty", &[]),
    ("foot", &[]),
    ("alacritty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("ghostty", &["-e"]),
    ("konsole", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("xfce4-terminal", &["-x"]),
    ("xterm", &["-e"]),
];

/// The command line prefix the command to run is appended to: the
/// configured one, then `xdg-terminal-exec`, then `$TERMINAL`, then the
/// first known terminal installed.
pub fn resolve(
    configured: Option<&[String]>,
    env_terminal: Option<&str>,
    checker: &impl BinaryChecker,
) -> Option<Vec<String>> {
    if let Some(configured) = configured.filter(|argv| !argv.is_empty()) {
        return Some(configured.to_vec());
    }
    if checker.exists("xdg-terminal-exec") {
        return Some(vec!["xdg-terminal-exec".to_string()]);
    }
    if let Some(mut argv) = env_terminal
        .map(|value| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|argv| !argv.is_empty())
    {
        if checker.exists(&argv[0]) {
            let name = std::path::Path::new(&argv[0])
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            let flags: &[&str] = KNOWN
                .iter()
                .find(|(known, _)| *known == name)
                .map_or(&["-e"], |(_, flags)| flags);
            argv.extend(flags.iter().map(|flag| flag.to_string()));
            return Some(argv);
        }
    }
    KNOWN
        .iter()
        .find(|(name, _)| checker.exists(name))
        .map(|(name, flags)| {
            std::iter::once(*name)
                .chain(flags.iter().copied())
                .map(str::to_string)
                .collect()
        })
}

/// [`resolve`] with `$TERMINAL` and the binaries on `PATH`.
pub fn resolve_default(configured: Option<&[String]>) -> Option<Vec<String>> {
    let env_terminal = std::env::var("TERMINAL").ok();
    resolve(
        configured,
        env_terminal.as_deref(),
        &crate::DefaultBinaryChecker,
    )
}

/// Run the shell command line `command` in the terminal. It is passed as an
/// argument rather than spliced into a script, so its quotes stay intact.
/// With `hold`, the window waits for Enter once the command exits.
pub fn shell_argv(prefix: &[String], shell: &str, command: &str, hold: bool) -> Vec<String> {
    let script = if hold {
        r#"eval "$1"; rc=$?; printf '\n[exited %s] press Enter to close' "$rc"; read -r _"#
    } else {
        r#"eval "$1""#
    };
    let mut argv = prefix.to_vec();
    argv.extend([
        shell.to_string(),
        "-c".to_string(),
        script.to_string(),
        "raffi".to_string(),
        command.to_string(),
    ]);
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Installed(&'static [&'static str]);

    impl BinaryChecker for Installed {
        fn exists(&self, binary: &str) -> bool {
            self.0.contains(&binary)
        }
    }

    fn strings(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn test_resolve_order() {
        let all = Installed(&["xdg-terminal-exec", "foot", "alacritty", "ghostty"]);
        let configured = strings(&["kitty", "--single-instance"]);
        assert_eq!(
            resolve(Some(&configured), Some("foot"), &all),
            Some(configured.clone())
        );
        assert_eq!(
            resolve(Some(&[]), Some("foot"), &all),
            Some(strings(&["xdg-terminal-exec"]))
        );

        let no_xdg = Installed(&["foot", "alacritty", "ghostty", "myterm"]);
        assert_eq!(
            resolve(None, Some("alacritty --class x"), &no_xdg),
            Some(strings(&["alacritty", "--class", "x", "-e"]))
        );
        assert_eq!(
            resolve(None, Some("myterm"), &no_xdg),
            Some(strings(&["myterm", "-e"]))
        );
        // A $TERMINAL that is not installed falls through to the list.
        assert_eq!(
            resolve(None, Some("missing"), &no_xdg),
            Some(strings(&["foot"]))
        );
        assert_eq!(
            resolve(None, None, &Installed(&["wezterm"])),
            Some(strings(&["wezterm", "start", "--"]))
        );
        assert_eq!(resolve(None, None, &Installed(&[])), None);
    }

    #[test]
    fn test_shell_argv_keeps_the_command_intact() {
        let prefix = strings(&["alacritty", "-e"]);
        let command = r#"echo "it's" $HOME | cat"#;
        let argv = shell_argv(&prefix, "bash", command, false);
        assert_eq!(
            argv,
            strings(&[
                "alacritty",
                "-e",
                "bash",
                "-c",
                r#"eval "$1""#,
                "raffi",
                command
            ])
        );
        let held = shell_argv(&prefix, "bash", command, true);
        assert!(held[4].starts_with(r#"eval "$1"; "#) && held[4].contains("read -r _"));
        assert_eq!(held.last().map(String::as_str), Some(command));

        let output = std::process::Command::new("sh")
            .args(&argv[3..])
            .env("HOME", "/home/x")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "it's /home/x\n");
    }
}
