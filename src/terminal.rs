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
        r#"( eval "$1" ); rc=$?; printf '\n[exited %s] press Enter to close' "$rc"; read -r _"#
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

/// Run `program` with `args` in the terminal, as for desktop entries with
/// `Terminal=true`.
pub fn exec_argv(prefix: &[String], program: &str, args: &[String]) -> Vec<String> {
    let mut argv = prefix.to_vec();
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());
    argv
}

/// A terminal that can open a new tab in its running instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Kitty,
    WezTerm,
}

impl TabKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::WezTerm => "WezTerm",
        }
    }
}

/// A running kitty or WezTerm to open a tab in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabTarget {
    pub kind: TabKind,
    /// kitty's `--to` address, or WezTerm's socket path.
    pub socket: String,
    /// The instance's process, to raise its window. Unknown when kitty's
    /// address was given in full.
    pub pid: Option<u32>,
}

/// The terminal `prefix` starts, when it can open tabs.
pub fn tab_kind(prefix: &[String]) -> Option<TabKind> {
    let program = prefix.first()?;
    match program.rsplit('/').next().unwrap_or(program) {
        "kitty" => Some(TabKind::Kitty),
        "wezterm" => Some(TabKind::WezTerm),
        _ => None,
    }
}

/// The running instance of the terminal `prefix` starts, if it is kitty
/// or WezTerm. `kitty_socket` overrides `listen_on` from kitty.conf.
pub fn find_tab_target(prefix: &[String], kitty_socket: Option<&str>) -> Option<TabTarget> {
    let kind = tab_kind(prefix)?;
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    if kind == TabKind::Kitty {
        if let Some(socket) = env("KITTY_LISTEN_ON") {
            let pid = env("KITTY_PID").and_then(|pid| pid.parse().ok());
            return Some(TabTarget { kind, socket, pid });
        }
    }
    let sockets = std::fs::read_to_string("/proc/net/unix")
        .map(|table| unix_socket_paths(&table))
        .unwrap_or_default();
    let target = match kind {
        TabKind::Kitty => {
            let address = match kitty_socket {
                Some(address) => address.to_string(),
                None => {
                    let dir = env("KITTY_CONFIG_DIRECTORY")
                        .or_else(|| env("XDG_CONFIG_HOME").map(|dir| format!("{dir}/kitty")))
                        .or_else(|| env("HOME").map(|home| format!("{home}/.config/kitty")))?;
                    let conf = std::fs::read_to_string(format!("{dir}/kitty.conf")).ok()?;
                    kitty_listen_on(&conf)?
                }
            };
            let home = env("HOME").unwrap_or_default();
            let address = expand_kitty_address(&address, &env, &home, &std::env::temp_dir());
            kitty_target(&address, &sockets, process_start_time)
        }
        TabKind::WezTerm => wezterm_target(&env("XDG_RUNTIME_DIR")?, &sockets, process_start_time),
    };
    crate::debug_log!("terminal: tab target {target:?}");
    target
}

/// The command line opening `command` in a new tab of `target`, wrapped
/// like [`shell_argv`] and started in `cwd`.
pub fn tab_argv(
    target: &TabTarget,
    prefix: &[String],
    shell: &str,
    command: &str,
    hold: bool,
    cwd: Option<&str>,
) -> Vec<String> {
    let program = prefix.first().map_or("", String::as_str);
    let mut argv: Vec<String> = match target.kind {
        TabKind::Kitty => [program, "@", "--to", &target.socket, "launch", "--type=tab"]
            .map(str::to_string)
            .to_vec(),
        TabKind::WezTerm => [
            "env".to_string(),
            format!("WEZTERM_UNIX_SOCKET={}", target.socket),
            program.to_string(),
            "cli".to_string(),
            "spawn".to_string(),
        ]
        .to_vec(),
    };
    if let Some(cwd) = cwd {
        argv.extend(["--cwd".to_string(), cwd.to_string()]);
    }
    if target.kind == TabKind::Kitty {
        argv.extend(["--title".to_string(), command.to_string()]);
    }
    argv.push("--".to_string());
    argv.extend(shell_argv(&[], shell, command, hold));
    argv
}

/// For a WezTerm [`tab_argv`], the command line listing its panes as JSON.
pub fn wezterm_list_argv(tab: &[String]) -> Option<Vec<String>> {
    let spawn = tab.iter().position(|arg| arg == "spawn")?;
    let mut argv = tab[..spawn].to_vec();
    argv.extend(["list", "--format", "json"].map(str::to_string));
    Some(argv)
}

/// The WezTerm `tab` command line naming the first window of `list`, the
/// output of [`wezterm_list_argv`]. Without it, WezTerm fails when it
/// cannot tell which window has focus.
pub fn wezterm_spawn_in_window(tab: &[String], list: &[u8]) -> Option<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Pane {
        window_id: u64,
    }
    let window = serde_json::from_slice::<Vec<Pane>>(list)
        .ok()?
        .first()?
        .window_id;
    let spawn = tab.iter().position(|arg| arg == "spawn")?;
    let mut argv = tab.to_vec();
    argv.splice(
        spawn + 1..spawn + 1,
        ["--window-id".to_string(), window.to_string()],
    );
    Some(argv)
}

/// The last `listen_on` of a kitty.conf, unless it is `none`.
fn kitty_listen_on(conf: &str) -> Option<String> {
    conf.lines()
        .filter_map(|line| {
            let mut words = line.trim().splitn(2, char::is_whitespace);
            (words.next() == Some("listen_on")).then(|| words.next().unwrap_or("").trim())
        })
        .next_back()
        .filter(|value| !value.is_empty() && *value != "none")
        .map(str::to_string)
}

/// `address` as kitty reads it: environment variables expanded, then a
/// socket path starting with `~` made absolute, and a relative one put in
/// the temporary directory.
fn expand_kitty_address(
    address: &str,
    env: &impl Fn(&str) -> Option<String>,
    home: &str,
    temp: &std::path::Path,
) -> String {
    let address = expand_vars(address, env);
    let Some(path) = address.strip_prefix("unix:") else {
        return address;
    };
    if path.starts_with('@') || path.starts_with('/') {
        address.clone()
    } else if let Some(rest) = path.strip_prefix('~') {
        format!("unix:{home}{rest}")
    } else {
        format!("unix:{}", temp.join(path).display())
    }
}

/// `$NAME` and `${NAME}` replaced by their value; unknown ones are kept.
fn expand_vars(value: &str, env: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let (name, len) = match after.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            },
            None => {
                let end = after
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .unwrap_or(after.len());
                (&after[..end], end)
            }
        };
        match env(name).filter(|_| !name.is_empty()) {
            Some(found) => out.push_str(&found),
            None => out.push_str(&rest[start..start + 1 + len]),
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// The paths of the Unix sockets in `/proc/net/unix`, abstract ones
/// starting with `@`.
fn unix_socket_paths(table: &str) -> Vec<String> {
    let mut paths: Vec<String> = table
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(7).map(str::to_string))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The live kitty listening on `address`: kitty adds `-<pid>` to it, or
/// puts the pid in place of `{kitty_pid}`. The newest instance wins; an
/// exact match is used when no pid is found.
fn kitty_target(
    address: &str,
    sockets: &[String],
    start_time: impl Fn(u32) -> Option<u64>,
) -> Option<TabTarget> {
    let Some(path) = address.strip_prefix("unix:") else {
        // A TCP address cannot be checked; a failed tab opens a window.
        return Some(TabTarget {
            kind: TabKind::Kitty,
            socket: address.to_string(),
            pid: None,
        });
    };
    let (before, after) = path
        .split_once("{kitty_pid}")
        .map_or((format!("{path}-"), ""), |(before, after)| {
            (before.to_string(), after)
        });
    if let Some((socket, pid)) = newest(sockets, &before, after, start_time) {
        return Some(TabTarget {
            kind: TabKind::Kitty,
            socket: format!("unix:{socket}"),
            pid: Some(pid),
        });
    }
    sockets
        .iter()
        .any(|socket| socket == path)
        .then(|| TabTarget {
            kind: TabKind::Kitty,
            socket: address.to_string(),
            pid: None,
        })
}

/// The newest live WezTerm GUI, from its `wezterm/gui-sock-<pid>` socket.
fn wezterm_target(
    runtime_dir: &str,
    sockets: &[String],
    start_time: impl Fn(u32) -> Option<u64>,
) -> Option<TabTarget> {
    let before = format!("{runtime_dir}/wezterm/gui-sock-");
    let (socket, pid) = newest(sockets, &before, "", start_time)?;
    Some(TabTarget {
        kind: TabKind::WezTerm,
        socket,
        pid: Some(pid),
    })
}

/// Among `sockets` named `<before><pid><after>`, the one of the most
/// recently started process still running.
fn newest(
    sockets: &[String],
    before: &str,
    after: &str,
    start_time: impl Fn(u32) -> Option<u64>,
) -> Option<(String, u32)> {
    sockets
        .iter()
        .filter_map(|socket| {
            let pid = socket.strip_prefix(before)?.strip_suffix(after)?;
            if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let pid: u32 = pid.parse().ok()?;
            Some((start_time(pid)?, socket.clone(), pid))
        })
        .max_by_key(|(started, ..)| *started)
        .map(|(_, socket, pid)| (socket, pid))
}

/// When process `pid` started, in clock ticks since boot; `None` once it
/// is gone.
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; the fields after
    // its closing parenthesis start at the third one.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
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
        assert!(held[4].starts_with(r#"( eval "$1" ); "#) && held[4].contains("read -r _"));
        assert_eq!(held.last().map(String::as_str), Some(command));

        let output = std::process::Command::new("sh")
            .args(&argv[3..])
            .env("HOME", "/home/x")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "it's /home/x\n");
    }

    #[test]
    fn test_hold_waits_whatever_the_command_does() {
        use std::io::Write;
        for shell in ["bash", "zsh", "sh"] {
            if !crate::find_binary(shell) {
                continue;
            }
            for (command, status) in [
                ("exit 7", 7),
                ("exec true", 0),
                ("set -e; false; echo unreachable", 1),
                ("exec </dev/null", 0),
            ] {
                let argv = shell_argv(&[], shell, command, true);
                let mut child = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "{shell}: {command} closed without waiting"
                );
                child.stdin.take().unwrap().write_all(b"\n").unwrap();
                let output = child.wait_with_output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(
                    stdout.contains(&format!("[exited {status}]")),
                    "{shell}: {command}: {stdout:?}"
                );
            }
        }
    }

    #[test]
    fn test_exec_argv() {
        assert_eq!(
            exec_argv(&strings(&["foot"]), "htop", &strings(&["-d", "10"])),
            strings(&["foot", "htop", "-d", "10"])
        );
    }

    #[test]
    fn test_tab_kind() {
        for (program, kind) in [
            ("kitty", Some(TabKind::Kitty)),
            ("/usr/bin/kitty", Some(TabKind::Kitty)),
            ("wezterm", Some(TabKind::WezTerm)),
            ("alacritty", None),
            ("xdg-terminal-exec", None),
        ] {
            assert_eq!(tab_kind(&strings(&[program, "-e"])), kind, "{program}");
        }
        assert_eq!(tab_kind(&[]), None);
    }

    #[test]
    fn test_kitty_listen_on() {
        let conf = "# listen_on unix:/tmp/old\nlisten_on unix:/tmp/a\n  listen_on   unix:@b  \n";
        assert_eq!(kitty_listen_on(conf).as_deref(), Some("unix:@b"));
        assert_eq!(kitty_listen_on("listen_on none\n"), None);
        assert_eq!(kitty_listen_on("font_size 12\n"), None);

        let env = |name: &str| (name == "XDG_RUNTIME_DIR").then(|| "/run/user/1".to_string());
        let temp = std::path::Path::new("/tmp");
        let expand = |address| expand_kitty_address(address, &env, "/home/me", temp);
        assert_eq!(expand("unix:${XDG_RUNTIME_DIR}/k"), "unix:/run/user/1/k");
        assert_eq!(expand("unix:$XDG_RUNTIME_DIR/k"), "unix:/run/user/1/k");
        assert_eq!(expand("unix:~/k"), "unix:/home/me/k");
        assert_eq!(expand("unix:mykitty"), "unix:/tmp/mykitty");
        assert_eq!(expand("unix:@$NOPE"), "unix:@$NOPE");
        assert_eq!(expand("tcp:localhost:1"), "tcp:localhost:1");
    }

    #[test]
    fn test_kitty_target_picks_the_newest_live_socket() {
        let table = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000: 00000002 00000000 00010000 0001 01 1 /tmp/mykitty-10\n\
            0000: 00000003 00000000 00000000 0001 03 2 /tmp/mykitty-10\n\
            0000: 00000002 00000000 00010000 0001 01 3 /tmp/mykitty-20\n\
            0000: 00000002 00000000 00010000 0001 01 4 /tmp/mykitty-30\n\
            0000: 00000002 00000000 00010000 0001 01 5 /tmp/mykitty-x-40\n\
            0000: 00000002 00000000 00010000 0001 01 6 @k-50.sock\n\
            0000: 00000002 00000000 00010000 0001 01 7\n";
        let sockets = unix_socket_paths(table);
        assert_eq!(
            sockets.iter().filter(|s| *s == "/tmp/mykitty-10").count(),
            1
        );
        // 30 is gone, 20 started after 10.
        let started = |pid| match pid {
            10 => Some(100),
            20 => Some(200),
            40 => Some(900),
            50 => Some(1),
            _ => None,
        };
        let kitty = |socket: &str, pid| TabTarget {
            kind: TabKind::Kitty,
            socket: socket.to_string(),
            pid,
        };
        assert_eq!(
            kitty_target("unix:/tmp/mykitty", &sockets, started),
            Some(kitty("unix:/tmp/mykitty-20", Some(20)))
        );
        assert_eq!(
            kitty_target("unix:@k-{kitty_pid}.sock", &sockets, started),
            Some(kitty("unix:@k-50.sock", Some(50)))
        );
        assert_eq!(
            kitty_target("unix:/tmp/mykitty-10", &sockets, |_| None),
            Some(kitty("unix:/tmp/mykitty-10", None)),
            "a full address matches as is"
        );
        assert_eq!(kitty_target("unix:/tmp/other", &sockets, started), None);
        assert_eq!(
            kitty_target("tcp:localhost:5", &sockets, started),
            Some(kitty("tcp:localhost:5", None))
        );
    }

    #[test]
    fn test_wezterm_target() {
        let sockets = strings(&[
            "/run/user/1/wezterm/gui-sock-7",
            "/run/user/1/wezterm/gui-sock-8",
            "/run/user/1/wezterm/sock",
        ]);
        let target = wezterm_target("/run/user/1", &sockets, |pid| (pid == 7).then_some(5));
        assert_eq!(
            target,
            Some(TabTarget {
                kind: TabKind::WezTerm,
                socket: "/run/user/1/wezterm/gui-sock-7".to_string(),
                pid: Some(7),
            })
        );
        assert_eq!(wezterm_target("/run/user/2", &sockets, |_| Some(1)), None);
    }

    #[test]
    fn test_tab_argv() {
        let kitty = TabTarget {
            kind: TabKind::Kitty,
            socket: "unix:/tmp/k-1".to_string(),
            pid: Some(1),
        };
        let script = "eval \"$1\"";
        assert_eq!(
            tab_argv(
                &kitty,
                &strings(&["/usr/bin/kitty"]),
                "zsh",
                "ls -l",
                false,
                Some("/home/me")
            ),
            strings(&[
                "/usr/bin/kitty",
                "@",
                "--to",
                "unix:/tmp/k-1",
                "launch",
                "--type=tab",
                "--cwd",
                "/home/me",
                "--title",
                "ls -l",
                "--",
                "zsh",
                "-c",
                script,
                "raffi",
                "ls -l",
            ])
        );
        let wezterm = TabTarget {
            kind: TabKind::WezTerm,
            socket: "/run/user/1/wezterm/gui-sock-7".to_string(),
            pid: Some(7),
        };
        assert_eq!(
            tab_argv(
                &wezterm,
                &strings(&["wezterm", "start", "--"]),
                "bash",
                "top",
                false,
                None
            ),
            strings(&[
                "env",
                "WEZTERM_UNIX_SOCKET=/run/user/1/wezterm/gui-sock-7",
                "wezterm",
                "cli",
                "spawn",
                "--",
                "bash",
                "-c",
                script,
                "raffi",
                "top",
            ])
        );

        let tab = tab_argv(&wezterm, &strings(&["wezterm"]), "bash", "top", false, None);
        assert_eq!(
            wezterm_list_argv(&tab).unwrap()[2..],
            strings(&["wezterm", "cli", "list", "--format", "json"])
        );
        let list = br#"[{"window_id": 3, "pane_id": 9}, {"window_id": 1, "pane_id": 2}]"#;
        assert_eq!(
            wezterm_spawn_in_window(&tab, list).unwrap()[3..8],
            strings(&["cli", "spawn", "--window-id", "3", "--"])
        );
        assert_eq!(wezterm_spawn_in_window(&tab, b"[]"), None);
    }
}
