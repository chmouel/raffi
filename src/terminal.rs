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
    let sockets = std::fs::read_to_string("/proc/net/unix")
        .map(|table| unix_sockets(&table))
        .unwrap_or_default();
    let target = match kind {
        TabKind::Kitty => {
            if let Some(address) = env("KITTY_LISTEN_ON") {
                let pid = env("KITTY_PID").and_then(|pid| pid.parse().ok());
                inherited_kitty(&address, pid, &sockets, &Proc)
            } else {
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
                kitty_target(&address, &sockets, &Proc)
            }
        }
        TabKind::WezTerm => wezterm_target(&env("XDG_RUNTIME_DIR")?, &sockets, &Proc),
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

/// The part of a [`tab_argv`] before `verb` (`launch` or `spawn`),
/// followed by `args`: another remote control command for the same
/// instance.
fn remote_argv(tab: &[String], verb: &str, args: &[&str]) -> Option<Vec<String>> {
    let at = tab.iter().position(|arg| arg == verb)?;
    let mut argv = tab[..at].to_vec();
    argv.extend(args.iter().map(|arg| arg.to_string()));
    Some(argv)
}

/// For a kitty [`tab_argv`], a read-only command that only succeeds when
/// the instance answers remote control.
pub fn kitty_check_argv(tab: &[String]) -> Option<Vec<String>> {
    remote_argv(tab, "launch", &["ls"])
}

/// For a WezTerm [`tab_argv`], the command line listing its panes as JSON.
pub fn wezterm_list_argv(tab: &[String]) -> Option<Vec<String>> {
    remote_argv(tab, "spawn", &["list", "--format", "json"])
}

/// For a WezTerm [`tab_argv`], the command line listing its clients, with
/// the pane each has focused, as JSON.
pub fn wezterm_clients_argv(tab: &[String]) -> Option<Vec<String>> {
    remote_argv(tab, "spawn", &["list-clients", "--format", "json"])
}

/// The WezTerm `tab` command line naming where the tab goes, so WezTerm
/// does not have to guess: next to the pane focused most recently in
/// `clients` (from [`wezterm_clients_argv`]), else in the first window of
/// `list` (from [`wezterm_list_argv`]).
pub fn wezterm_spawn_argv(
    tab: &[String],
    clients: Option<&[u8]>,
    list: &[u8],
) -> Option<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Idle {
        secs: u64,
        nanos: u32,
    }
    #[derive(serde::Deserialize)]
    struct Client {
        focused_pane_id: Option<u64>,
        idle_time: Option<Idle>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        window_id: u64,
    }
    let focused = clients
        .and_then(|clients| serde_json::from_slice::<Vec<Client>>(clients).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|client| {
            let idle = client
                .idle_time
                .map_or((u64::MAX, 0), |idle| (idle.secs, idle.nanos));
            Some((idle, client.focused_pane_id?))
        })
        .min_by_key(|(idle, _)| *idle)
        .map(|(_, pane)| ("--pane-id", pane));
    let place = match focused {
        Some(place) => place,
        None => (
            "--window-id",
            serde_json::from_slice::<Vec<Pane>>(list)
                .ok()?
                .first()?
                .window_id,
        ),
    };
    let spawn = tab.iter().position(|arg| arg == "spawn")?;
    let mut argv = tab.to_vec();
    argv.splice(
        spawn + 1..spawn + 1,
        [place.0.to_string(), place.1.to_string()],
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

/// A Unix socket from `/proc/net/unix`: its path, abstract ones starting
/// with `@`, and its inode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnixSocket {
    path: String,
    inode: u64,
}

fn unix_sockets(table: &str) -> Vec<UnixSocket> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            Some(UnixSocket {
                path: fields.get(7)?.to_string(),
                inode: fields.get(6)?.parse().ok()?,
            })
        })
        .collect()
}

/// What tab discovery needs to know about processes and socket files.
trait Processes {
    /// When `pid` started, in clock ticks since boot; `None` once it is gone.
    fn start_time(&self, pid: u32) -> Option<u64>;
    /// Whether `pid` is one of ours and holds socket `inode` open.
    fn holds(&self, pid: u32, inode: u64) -> bool;
    /// Whether the socket file `path` belongs to us.
    fn owns(&self, path: &str) -> bool;
}

struct Proc;

impl Processes for Proc {
    fn start_time(&self, pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may hold spaces and parentheses; the fields
        // after its closing parenthesis start at the third one.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)?
            .parse()
            .ok()
    }

    fn holds(&self, pid: u32, inode: u64) -> bool {
        // Only the fds of our own processes can be listed, so this also
        // checks that `pid` is ours.
        let target = format!("socket:[{inode}]");
        std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|fds| {
            fds.flatten().any(|fd| {
                std::fs::read_link(fd.path()).is_ok_and(|link| link.as_os_str() == &*target)
            })
        })
    }

    fn owns(&self, path: &str) -> bool {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        std::fs::symlink_metadata(path)
            .is_ok_and(|meta| meta.file_type().is_socket() && meta.uid() == uid)
    }
}

/// The kitty Raffi runs inside, from its `$KITTY_LISTEN_ON` and
/// `$KITTY_PID`, checked like a discovered one.
fn inherited_kitty(
    address: &str,
    pid: Option<u32>,
    sockets: &[UnixSocket],
    procs: &impl Processes,
) -> Option<TabTarget> {
    let path = address.strip_prefix("unix:")?;
    let trusted = match pid {
        Some(pid) => sockets
            .iter()
            .any(|socket| socket.path == path && procs.holds(pid, socket.inode)),
        None => !path.starts_with('@') && procs.owns(path),
    };
    trusted.then(|| TabTarget {
        kind: TabKind::Kitty,
        socket: address.to_string(),
        pid,
    })
}

/// The live kitty of ours listening on `address`: kitty adds `-<pid>` to
/// it, or puts the pid in place of `{kitty_pid}`. The newest instance wins;
/// an exact match on a socket file we own is used when no pid is found.
/// Other addresses, such as TCP, cannot be checked and are not used.
fn kitty_target(
    address: &str,
    sockets: &[UnixSocket],
    procs: &impl Processes,
) -> Option<TabTarget> {
    let path = address.strip_prefix("unix:")?;
    let (before, after) = path
        .split_once("{kitty_pid}")
        .map_or((format!("{path}-"), ""), |(before, after)| {
            (before.to_string(), after)
        });
    if let Some((socket, pid)) = newest(sockets, &before, after, procs) {
        return Some(TabTarget {
            kind: TabKind::Kitty,
            socket: format!("unix:{socket}"),
            pid: Some(pid),
        });
    }
    (!path.starts_with('@') && sockets.iter().any(|socket| socket.path == path) && procs.owns(path))
        .then(|| TabTarget {
            kind: TabKind::Kitty,
            socket: address.to_string(),
            pid: None,
        })
}

/// The newest live WezTerm GUI of ours, from its `wezterm/gui-sock-<pid>`
/// socket.
fn wezterm_target(
    runtime_dir: &str,
    sockets: &[UnixSocket],
    procs: &impl Processes,
) -> Option<TabTarget> {
    let before = format!("{runtime_dir}/wezterm/gui-sock-");
    let (socket, pid) = newest(sockets, &before, "", procs)?;
    Some(TabTarget {
        kind: TabKind::WezTerm,
        socket,
        pid: Some(pid),
    })
}

/// Among `sockets` named `<before><pid><after>` and held by that process,
/// the one of the most recently started process still running.
fn newest(
    sockets: &[UnixSocket],
    before: &str,
    after: &str,
    procs: &impl Processes,
) -> Option<(String, u32)> {
    sockets
        .iter()
        .filter_map(|socket| {
            let pid = socket.path.strip_prefix(before)?.strip_suffix(after)?;
            if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let pid: u32 = pid.parse().ok()?;
            let started = procs.start_time(pid)?;
            procs
                .holds(pid, socket.inode)
                .then(|| (started, socket.path.clone(), pid))
        })
        .max_by_key(|(started, ..)| *started)
        .map(|(_, socket, pid)| (socket, pid))
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

    /// Fake processes: `(pid, start time, socket inodes held)`, and the
    /// socket files we own.
    struct FakeProcs(Vec<(u32, u64, Vec<u64>)>, Vec<&'static str>);

    impl Processes for FakeProcs {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.iter().find(|p| p.0 == pid).map(|p| p.1)
        }
        fn holds(&self, pid: u32, inode: u64) -> bool {
            self.0.iter().any(|p| p.0 == pid && p.2.contains(&inode))
        }
        fn owns(&self, path: &str) -> bool {
            self.1.contains(&path)
        }
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
        let sockets = unix_sockets(table);
        assert_eq!(
            sockets[0],
            UnixSocket {
                path: "/tmp/mykitty-10".to_string(),
                inode: 1
            }
        );
        // 30 is gone, 20 started after 10.
        let procs = FakeProcs(
            vec![
                (10, 100, vec![1, 2]),
                (20, 200, vec![3]),
                (40, 900, vec![5]),
                (50, 1, vec![6]),
            ],
            vec!["/tmp/mykitty-10"],
        );
        let kitty = |socket: &str, pid| TabTarget {
            kind: TabKind::Kitty,
            socket: socket.to_string(),
            pid,
        };
        assert_eq!(
            kitty_target("unix:/tmp/mykitty", &sockets, &procs),
            Some(kitty("unix:/tmp/mykitty-20", Some(20)))
        );
        assert_eq!(
            kitty_target("unix:@k-{kitty_pid}.sock", &sockets, &procs),
            Some(kitty("unix:@k-50.sock", Some(50)))
        );
        let none = FakeProcs(vec![], vec!["/tmp/mykitty-10"]);
        assert_eq!(
            kitty_target("unix:/tmp/mykitty-10", &sockets, &none),
            Some(kitty("unix:/tmp/mykitty-10", None)),
            "a full address matches as is"
        );
        assert_eq!(kitty_target("unix:/tmp/other", &sockets, &procs), None);
        assert_eq!(kitty_target("tcp:localhost:5", &sockets, &procs), None);
    }

    #[test]
    fn test_kitty_target_skips_sockets_of_other_users() {
        let sockets = unix_sockets(
            "Num RefCount Protocol Flags Type St Inode Path\n\
            0000: 00000002 00000000 00010000 0001 01 1 /tmp/mykitty-10\n\
            0000: 00000002 00000000 00010000 0001 01 2 /tmp/mykitty-20\n\
            0000: 00000002 00000000 00010000 0001 01 3 @k-20\n",
        );
        // 20 is newer and ours, but another user made its sockets: it does
        // not hold them.
        let procs = FakeProcs(vec![(10, 100, vec![1]), (20, 200, vec![9])], vec![]);
        assert_eq!(
            kitty_target("unix:/tmp/mykitty", &sockets, &procs).map(|t| t.pid),
            Some(Some(10))
        );
        assert_eq!(kitty_target("unix:@k", &sockets, &procs), None);
        assert_eq!(
            kitty_target("unix:/tmp/mykitty-20", &sockets, &procs),
            None,
            "a full address needs a socket file we own"
        );
        assert_eq!(
            kitty_target("unix:@k-20", &sockets, &FakeProcs(vec![], vec!["@k-20"])),
            None,
            "an abstract socket without a pid cannot be checked"
        );
    }

    #[test]
    fn test_wezterm_target() {
        let sockets = unix_sockets(
            "Num RefCount Protocol Flags Type St Inode Path\n\
            0000: 00000002 00000000 00010000 0001 01 1 /run/user/1/wezterm/gui-sock-7\n\
            0000: 00000002 00000000 00010000 0001 01 2 /run/user/1/wezterm/gui-sock-8\n\
            0000: 00000002 00000000 00010000 0001 01 3 /run/user/1/wezterm/sock\n",
        );
        let procs = FakeProcs(vec![(7, 5, vec![1])], vec![]);
        let target = wezterm_target("/run/user/1", &sockets, &procs);
        assert_eq!(
            target,
            Some(TabTarget {
                kind: TabKind::WezTerm,
                socket: "/run/user/1/wezterm/gui-sock-7".to_string(),
                pid: Some(7),
            })
        );
        assert_eq!(wezterm_target("/run/user/2", &sockets, &procs), None);
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
        assert_eq!(
            wezterm_clients_argv(&tab).unwrap()[2..],
            strings(&["wezterm", "cli", "list-clients", "--format", "json"])
        );
        let list = br#"[{"window_id": 3, "pane_id": 9}, {"window_id": 1, "pane_id": 2}]"#;
        let clients = br#"[
            {"focused_pane_id": 4, "idle_time": {"secs": 60, "nanos": 0}},
            {"focused_pane_id": 2, "idle_time": {"secs": 5, "nanos": 0}},
            {"focused_pane_id": null, "idle_time": {"secs": 0, "nanos": 0}}
        ]"#;
        assert_eq!(
            wezterm_spawn_argv(&tab, Some(clients), list).unwrap()[3..8],
            strings(&["cli", "spawn", "--pane-id", "2", "--"]),
            "next to the pane focused last"
        );
        assert_eq!(
            wezterm_spawn_argv(&tab, Some(b"[]"), list).unwrap()[3..8],
            strings(&["cli", "spawn", "--window-id", "3", "--"])
        );
        assert_eq!(wezterm_spawn_argv(&tab, None, b"[]"), None);

        let tab = tab_argv(&kitty, &strings(&["kitty"]), "zsh", "ls", false, None);
        assert_eq!(
            kitty_check_argv(&tab).unwrap(),
            strings(&["kitty", "@", "--to", "unix:/tmp/k-1", "ls"])
        );
    }

    #[test]
    fn test_inherited_kitty_is_checked() {
        let sockets = unix_sockets(
            "Num RefCount Protocol Flags Type St Inode Path\n\
            0000: 00000002 00000000 00010000 0001 01 1 @kitty-10\n",
        );
        let procs = FakeProcs(vec![(10, 1, vec![1])], vec![]);
        assert_eq!(
            inherited_kitty("unix:@kitty-10", Some(10), &sockets, &procs).map(|t| t.pid),
            Some(Some(10))
        );
        assert_eq!(
            inherited_kitty("unix:@kitty-10", Some(11), &sockets, &procs),
            None
        );
        assert_eq!(
            inherited_kitty("unix:@kitty-10", None, &sockets, &procs),
            None
        );
        assert_eq!(
            inherited_kitty("tcp:localhost:1", Some(10), &sockets, &procs),
            None
        );
    }

    #[test]
    fn test_proc_owns_our_socket_files_only() {
        let dir = std::env::temp_dir().join(format!("raffi-owns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        assert!(Proc.owns(socket.to_str().unwrap()));
        assert!(!Proc.owns(file.to_str().unwrap()), "not a socket");
        assert!(!Proc.owns(dir.join("gone").to_str().unwrap()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
