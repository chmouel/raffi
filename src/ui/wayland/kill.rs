use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::actions::{run_bounded, DeferredAction};
use super::command_list::{CommandAction, CommandItem};
use super::preview::format_byte_size;

const PROC: &str = "/proc";
const LOAD_BUDGET: Duration = Duration::from_secs(2);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
const SUBTITLE_COMMAND_CHARS: usize = 100;
const DETAILS_COMMAND_CHARS: usize = 1000;
const FALLBACK_ICON: &str = "application-x-executable";
pub(super) const NO_PROCESSES: &str = "No processes found";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Term,
    Kill,
}

impl Signal {
    fn flag(self) -> &'static str {
        match self {
            Self::Term => "-TERM",
            Self::Kill => "-KILL",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }
}

/// A process as listed, so the signal can be refused when the pid now
/// belongs to another process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SignalTarget {
    pub pid: u32,
    pub start_time: u64,
    pub uid: u32,
    pub signal: Signal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Process {
    pub pid: u32,
    /// `comm` from `/proc/<pid>/stat`, at most 15 bytes.
    pub name: String,
    /// File name of the first command line argument, used for the icon.
    pub program: String,
    pub command: String,
    pub rss_kib: u64,
    pub start_time: u64,
    pub uid: u32,
}

#[derive(Debug, PartialEq, Eq)]
struct Stat {
    name: String,
    state: char,
    start_time: u64,
}

/// Parse `/proc/<pid>/stat`. Fields are read after the last `)`, since the
/// name between the parentheses can hold spaces and parentheses itself.
fn parse_stat(stat: &str) -> Option<Stat> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let mut fields = stat[close + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    // Field 22 of the whole line; the state is field 3.
    let start_time = fields.nth(18)?.parse().ok()?;
    Some(Stat {
        name,
        state,
        start_time,
    })
}

/// The real uid and resident memory in KiB from `/proc/<pid>/status`.
fn parse_status(status: &str) -> (Option<u32>, Option<u64>) {
    let field = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
    };
    (
        field("Uid:").and_then(|uid| uid.parse().ok()),
        field("VmRSS:").and_then(|rss| rss.parse().ok()),
    )
}

fn read_uid(dir: &Path) -> Option<u32> {
    parse_status(&fs::read_to_string(dir.join("status")).ok()?).0
}

/// The process `pid` under `root`, when it is a live user process owned by
/// `uid`. Kernel threads have no command line.
fn read_process(root: &Path, pid: u32, uid: u32) -> Option<Process> {
    let dir = root.join(pid.to_string());
    let (owner, rss_kib) = parse_status(&fs::read_to_string(dir.join("status")).ok()?);
    if owner != Some(uid) {
        return None;
    }
    let stat = parse_stat(&fs::read_to_string(dir.join("stat")).ok()?)?;
    if stat.state == 'Z' {
        return None;
    }
    let cmdline = fs::read(dir.join("cmdline")).ok()?;
    let args: Vec<String> = cmdline
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let first = args.first()?;
    let program = first
        .split_whitespace()
        .next()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or_default()
        .trim_start_matches('-')
        .to_string();
    Some(Process {
        pid,
        name: stat.name,
        program,
        command: args.join(" "),
        rss_kib: rss_kib.unwrap_or(0),
        start_time: stat.start_time,
        uid,
    })
}

/// The processes under `root` sharing the real uid of `root/self`, except
/// `exclude`, largest memory first. Stops at `deadline` with what it has.
pub(super) fn list_processes(
    root: &Path,
    exclude: &[u32],
    deadline: Instant,
) -> Result<Vec<Process>, String> {
    let uid = read_uid(&root.join("self")).ok_or("cannot read your user id from /proc")?;
    let entries = fs::read_dir(root).map_err(|error| format!("cannot list /proc: {error}"))?;
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            crate::debug_log!("kill: listing stopped at the deadline");
            break;
        }
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if exclude.contains(&pid) {
            continue;
        }
        processes.extend(read_process(root, pid, uid));
    }
    processes.sort_by(|a, b| b.rss_kib.cmp(&a.rss_kib).then(a.pid.cmp(&b.pid)));
    Ok(processes)
}

/// List the user's processes, except Raffi and its parent. Blocking: run it
/// off the UI thread.
pub(super) fn load_processes(keep_open: bool) -> Result<Vec<CommandItem>, String> {
    let exclude = [std::process::id(), std::os::unix::process::parent_id()];
    let processes = list_processes(Path::new(PROC), &exclude, Instant::now() + LOAD_BUDGET)?;
    Ok(process_items_with(&processes, keep_open))
}

#[allow(dead_code)]
pub(super) fn process_items(processes: &[Process]) -> Vec<CommandItem> {
    process_items_with(processes, true)
}

pub(super) fn process_items_with(processes: &[Process], keep_open: bool) -> Vec<CommandItem> {
    if processes.is_empty() {
        return vec![CommandItem::info(
            "empty",
            NO_PROCESSES.to_string(),
            "dialog-information",
        )];
    }
    processes
        .iter()
        .map(|process| process_item(process, keep_open))
        .collect()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max - 1).collect();
    short.push('…');
    short
}

fn process_item(process: &Process, keep_open: bool) -> CommandItem {
    let memory = format_byte_size(process.rss_kib * 1024);
    let action = |signal: Signal| CommandAction {
        action: DeferredAction::Signal(SignalTarget {
            pid: process.pid,
            start_time: process.start_time,
            uid: process.uid,
            signal,
        }),
        confirm: Some(format!("send {} to {}", signal.name(), process.name)),
        keep_open,
        progress: None,
    };
    let mut icon_names = vec![process.program.clone()];
    if process.name != process.program {
        icon_names.push(process.name.clone());
    }
    icon_names.retain(|name| !name.is_empty());
    icon_names.push(FALLBACK_ICON.to_string());
    CommandItem {
        title: process.name.clone(),
        subtitle: format!(
            "pid {} · {memory} · {}",
            process.pid,
            truncate(&process.command, SUBTITLE_COMMAND_CHARS)
        ),
        match_text: format!("{} {} {}", process.name, process.pid, process.command),
        icon_names,
        icon: None,
        identity: format!("{}/{}", process.pid, process.start_time),
        unavailable: None,
        primary: Some(action(Signal::Term)),
        secondary: Some(action(Signal::Kill)),
        details: vec![
            ("Pid".to_string(), process.pid.to_string()),
            ("Memory".to_string(), memory),
            (
                "Command".to_string(),
                truncate(&process.command, DETAILS_COMMAND_CHARS),
            ),
            (
                "Enter".to_string(),
                "Press twice to send SIGTERM".to_string(),
            ),
            (
                "Ctrl+Enter".to_string(),
                "Press twice to send SIGKILL".to_string(),
            ),
        ],
        badge: None,
        domains: Vec::new(),
        dimmed: false,
    }
}

/// Refuse when the pid under `root` is gone or no longer the process that
/// was listed.
fn check_target(root: &Path, target: &SignalTarget) -> Result<(), String> {
    let pid = target.pid;
    let dir = root.join(pid.to_string());
    let gone = || format!("process {pid} has exited");
    let stat = fs::read_to_string(dir.join("stat")).map_err(|_| gone())?;
    let stat = parse_stat(&stat).ok_or_else(gone)?;
    if stat.state == 'Z' {
        return Err(gone());
    }
    if stat.start_time != target.start_time {
        return Err(format!("pid {pid} now belongs to another process"));
    }
    if read_uid(&dir).ok_or_else(gone)? != target.uid {
        return Err(format!("pid {pid} now belongs to another user"));
    }
    Ok(())
}

/// Check the target under `root` again, then hand `kill`'s arguments to
/// `run`. A failed `kill` reports the process gone when the check now fails.
pub(super) fn send_signal_in(
    root: &Path,
    target: &SignalTarget,
    run: impl FnOnce(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    check_target(root, target)?;
    let argv = [
        "kill".to_string(),
        target.signal.flag().to_string(),
        target.pid.to_string(),
    ];
    run(&argv).map_err(|error| match check_target(root, target) {
        Err(gone) => gone,
        Ok(()) => format!("kill: {error}"),
    })
}

/// Signal the target after checking it again in `/proc`. Best effort: the
/// pid can still be reused between the check and the signal. Blocking: run
/// it off the UI thread.
pub(super) fn send_signal(target: &SignalTarget) -> Result<(), String> {
    send_signal_in(Path::new(PROC), target, |argv| {
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        run_bounded(command, KILL_TIMEOUT, 4096).map(|_| ())
    })
}

#[cfg(test)]
pub(super) fn fixture_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proc")
}

#[cfg(test)]
pub(super) fn fixture_items() -> Vec<CommandItem> {
    let processes = list_processes(&fixture_root(), &[400], Instant::now() + LOAD_BUDGET).unwrap();
    process_items(&processes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(pid: u32, start_time: u64, uid: u32) -> SignalTarget {
        SignalTarget {
            pid,
            start_time,
            uid,
            signal: Signal::Term,
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn test_parse_stat_reads_after_the_last_parenthesis() {
        let stat = parse_stat(
            "200 (a) b (c) S 1 200 200 0 -1 4194304 10 0 0 0 1 1 0 0 20 0 1 0 5000 1000000 100",
        )
        .unwrap();
        assert_eq!(
            stat,
            Stat {
                name: "a) b (c".into(),
                state: 'S',
                start_time: 5000,
            }
        );
        assert_eq!(
            parse_stat("1601559 (cat) R 1601556 1601556 1601556 0 -1 4194304 90 0 0 0 0 0 0 0 20 0 1 0 13627901 6414336 965")
                .map(|stat| stat.start_time),
            Some(13627901)
        );
        assert_eq!(parse_stat("1 (short) S 1 2 3"), None);
        assert_eq!(parse_stat(""), None);
        assert_eq!(parse_stat("1 )x( S"), None);
    }

    #[test]
    fn test_parse_status_reads_real_uid_and_rss() {
        let status = "Name:\tfoo\nUid:\t1000\t0\t0\t0\nVmRSS:\t    4476 kB\n";
        assert_eq!(parse_status(status), (Some(1000), Some(4476)));
        assert_eq!(parse_status("Name:\tkthread\n"), (None, None));
    }

    #[test]
    fn test_list_processes_keeps_the_users_live_processes() {
        let processes = list_processes(&fixture_root(), &[400], far()).unwrap();
        let summary: Vec<_> = processes
            .iter()
            .map(|process| (process.pid, process.name.as_str(), process.rss_kib))
            .collect();
        // Other uid (300), zombie (301), kernel thread (302), vanished (303)
        // and excluded (400) processes are left out; biggest first.
        assert_eq!(
            summary,
            vec![
                (201, "firefox", 512000),
                (202, "bash", 4096),
                (200, "a) b (c", 2048)
            ]
        );
        assert_eq!(processes[0].command, "/usr/lib/firefox/firefox -P default");
        assert_eq!(processes[0].program, "firefox");
        assert_eq!(processes[1].program, "bash");
        assert_eq!(processes[2].command, "/usr/bin/odd tool --name a) b (c");
        assert_eq!(processes[2].program, "odd");
        assert_eq!(processes[2].start_time, 5000);

        let processes = list_processes(&fixture_root(), &[], far()).unwrap();
        assert!(processes.iter().any(|process| process.pid == 400));

        let processes = list_processes(&fixture_root(), &[], Instant::now()).unwrap();
        assert!(processes.is_empty(), "nothing is read past the deadline");
    }

    #[test]
    fn test_process_items_rows_and_actions() {
        let items = fixture_items();
        let item = &items[2];
        assert_eq!(item.title, "a) b (c");
        assert_eq!(
            item.subtitle,
            "pid 200 · 2.0 MiB · /usr/bin/odd tool --name a) b (c"
        );
        assert_eq!(item.identity, "200/5000");
        assert_eq!(item.icon_names, vec!["odd", "a) b (c", FALLBACK_ICON]);
        let primary = item.primary.as_ref().unwrap();
        assert_eq!(primary.confirm.as_deref(), Some("send SIGTERM to a) b (c"));
        assert!(primary.keep_open);
        assert!(matches!(
            &primary.action,
            DeferredAction::Signal(SignalTarget {
                pid: 200,
                start_time: 5000,
                uid: 1000,
                signal: Signal::Term
            })
        ));
        let secondary = item.secondary.as_ref().unwrap();
        assert_eq!(
            secondary.confirm.as_deref(),
            Some("send SIGKILL to a) b (c")
        );
        assert!(matches!(
            &secondary.action,
            DeferredAction::Signal(SignalTarget {
                signal: Signal::Kill,
                ..
            })
        ));
        assert_eq!(items[1].icon_names, vec!["bash", FALLBACK_ICON]);

        let empty = process_items(&[]);
        assert_eq!(empty[0].title, NO_PROCESSES);
        assert!(empty[0].primary.is_none());

        let long = "x".repeat(300);
        assert_eq!(truncate(&long, 100).chars().count(), 100);
        assert!(truncate(&long, 100).ends_with('…'));
    }

    #[test]
    fn test_send_signal_checks_the_process_again() {
        let root = fixture_root();
        let refused = |target: SignalTarget| {
            send_signal_in(&root, &target, |_| panic!("must not signal")).unwrap_err()
        };
        assert_eq!(refused(target(999, 1, 1000)), "process 999 has exited");
        assert_eq!(refused(target(303, 1, 1000)), "process 303 has exited");
        assert_eq!(refused(target(301, 8000, 1000)), "process 301 has exited");
        assert_eq!(
            refused(target(200, 4999, 1000)),
            "pid 200 now belongs to another process"
        );
        assert_eq!(
            refused(target(300, 100, 1000)),
            "pid 300 now belongs to another user"
        );

        let mut ran = Vec::new();
        let mut kill = target(200, 5000, 1000);
        kill.signal = Signal::Kill;
        send_signal_in(&root, &kill, |argv| {
            ran = argv.to_vec();
            Ok(())
        })
        .unwrap();
        assert_eq!(ran, vec!["kill", "-KILL", "200"]);

        let error =
            send_signal_in(&root, &kill, |_| Err("exited with exit status: 1".into())).unwrap_err();
        assert_eq!(error, "kill: exited with exit status: 1");
    }

    #[test]
    fn test_live_sleep_child_is_listed_and_terminated() {
        use std::os::unix::process::ExitStatusExt;

        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        let process = loop {
            let found = list_processes(Path::new(PROC), &[std::process::id()], far())
                .unwrap()
                .into_iter()
                .find(|process| process.pid == pid && process.program == "sleep");
            match found {
                Some(process) => break process,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("sleep child {pid} not listed");
                }
            }
        };
        assert_eq!(process.command, "sleep 60");

        let item = process_item(&process, true);
        let action = item.primary.unwrap().action;
        let result = super::super::command_list::run_kept_open(&action);
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(status.signal(), Some(15));
    }
}
