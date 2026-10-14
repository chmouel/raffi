//! Clipboard history for `{clipboard:N}`, read from whichever clipboard
//! manager the user runs: cliphist, CopyQ or clipman.
//!
//! Raffi keeps no history of its own. The manager is found by looking at the
//! user's running processes, because an installed binary or a leftover
//! history file says nothing about which manager is actually recording.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::actions::run_bounded;
use super::placeholders::MAX_VALUE;
use crate::ClipboardHistory;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound for `cliphist list` output and clipman's history file.
const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const CLIPMAN_DEFAULT_PATH: &str = "~/.local/share/clipman.json";
const NO_MANAGER: &str = "no clipboard manager found: run cliphist, CopyQ or clipman, \
     or set general.clipboard_history";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Backend {
    /// Extra flags such as `-db-path`, copied from the running `cliphist store`.
    Cliphist {
        flags: Vec<String>,
    },
    CopyQ,
    Clipman {
        path: PathBuf,
    },
}

impl Backend {
    fn kind(&self) -> ClipboardHistory {
        match self {
            Self::Cliphist { .. } => ClipboardHistory::Cliphist,
            Self::CopyQ => ClipboardHistory::Copyq,
            Self::Clipman { .. } => ClipboardHistory::Clipman,
        }
    }
}

/// Pick the backend for `setting`, scanning running processes when needed.
pub(super) fn resolve(setting: ClipboardHistory) -> Result<Backend, String> {
    if setting == ClipboardHistory::None {
        return Err("clipboard history is disabled (general.clipboard_history: none)".into());
    }
    let backend = detect_from_cmdlines(&running_cmdlines(), setting);
    crate::debug_log!("clipboard history: setting={setting:?} backend={backend:?}");
    backend.ok_or_else(|| NO_MANAGER.to_string())
}

/// Read the requested history entries (newest is 0) from `backend`.
pub(super) fn read_entries(
    backend: &Backend,
    indices: &[usize],
) -> BTreeMap<usize, Result<String, String>> {
    let all = |error: String| {
        indices
            .iter()
            .map(|&index| (index, Err(error.clone())))
            .collect()
    };
    match backend {
        Backend::Cliphist { flags } => {
            let list = match run_cliphist(flags, &["list"], MAX_HISTORY_BYTES) {
                Ok(list) => list,
                Err(error) => return all(error),
            };
            let list = String::from_utf8_lossy(&list);
            indices
                .iter()
                .map(|&index| {
                    let entry = cliphist_id(&list, index).and_then(|id| {
                        let bytes = run_cliphist(flags, &["decode", id], MAX_VALUE)?;
                        text_entry(bytes, index)
                    });
                    (index, entry)
                })
                .collect()
        }
        Backend::CopyQ => {
            let count =
                match run("copyq", ["count"], 64).and_then(|output| parse_copyq_count(&output)) {
                    Ok(count) => count,
                    Err(error) => return all(error),
                };
            indices
                .iter()
                .map(|&index| {
                    let entry = if index >= count {
                        Err(only_entries(count))
                    } else {
                        let index_arg = index.to_string();
                        run("copyq", ["read", "text/plain", &index_arg], MAX_VALUE)
                            .and_then(|bytes| text_entry(bytes, index))
                    };
                    (index, entry)
                })
                .collect()
        }
        Backend::Clipman { path } => {
            let contents = match read_capped(path) {
                Ok(contents) => contents,
                Err(error) => return all(error),
            };
            indices
                .iter()
                .map(|&index| (index, clipman_entry(&contents, index)))
                .collect()
        }
    }
}

fn run<S: AsRef<OsStr>>(
    program: &str,
    args: impl IntoIterator<Item = S>,
    max_output: usize,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    command.args(args);
    run_bounded(command, COMMAND_TIMEOUT, max_output).map_err(|error| format!("{program}: {error}"))
}

fn run_cliphist(flags: &[String], args: &[&str], max_output: usize) -> Result<Vec<u8>, String> {
    let args = flags.iter().map(String::as_str).chain(args.iter().copied());
    run("cliphist", args, max_output)
}

fn text_entry(bytes: Vec<u8>, index: usize) -> Result<String, String> {
    match String::from_utf8(bytes) {
        Ok(text) if !text.is_empty() => Ok(text),
        _ => Err(format!("entry {index} is not text")),
    }
}

fn only_entries(count: usize) -> String {
    format!("history has only {count} entries")
}

/// The cliphist id on line `index` of `cliphist list` (newest first).
fn cliphist_id(list: &str, index: usize) -> Result<&str, String> {
    let line = list
        .lines()
        .nth(index)
        .ok_or_else(|| only_entries(list.lines().count()))?;
    let (id, preview) = line.split_once('\t').unwrap_or((line, ""));
    if preview.starts_with("[[ binary data") {
        return Err(format!("entry {index} is not text"));
    }
    Ok(id)
}

fn parse_copyq_count(output: &[u8]) -> Result<usize, String> {
    String::from_utf8_lossy(output)
        .trim()
        .parse()
        .map_err(|_| "copyq count: unexpected output".to_string())
}

/// Entry `index` of clipman's history file, a JSON list with the newest last.
fn clipman_entry(contents: &str, index: usize) -> Result<String, String> {
    let history: Vec<String> = serde_json::from_str(contents)
        .map_err(|error| format!("invalid clipman history: {error}"))?;
    history
        .len()
        .checked_sub(index + 1)
        .map(|position| history[position].clone())
        .ok_or_else(|| only_entries(history.len()))
}

fn read_capped(path: &Path) -> Result<String, String> {
    let display = path.display();
    let size = fs::metadata(path)
        .map_err(|error| format!("cannot read {display}: {error}"))?
        .len();
    if size > MAX_HISTORY_BYTES as u64 {
        return Err(format!("{display} is too large"));
    }
    fs::read_to_string(path).map_err(|error| format!("cannot read {display}: {error}"))
}

/// Command lines of the current user's processes.
fn running_cmdlines() -> Vec<Vec<String>> {
    let Ok(uid) = fs::metadata("/proc/self").map(|meta| meta.uid()) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter(|entry| entry.metadata().is_ok_and(|meta| meta.uid() == uid))
        .filter_map(|entry| fs::read(entry.path().join("cmdline")).ok())
        .map(|raw| {
            raw.split(|&byte| byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect::<Vec<_>>()
        })
        .filter(|args| !args.is_empty())
        .collect()
}

/// Choose a backend from process command lines. With `Auto`, the first of
/// cliphist, CopyQ and clipman that is running wins. An explicit setting
/// always yields that backend, taking custom paths from its running process
/// when there is one.
fn detect_from_cmdlines(cmdlines: &[Vec<String>], setting: ClipboardHistory) -> Option<Backend> {
    let running: Vec<Backend> = cmdlines
        .iter()
        .filter_map(|args| parse_cmdline(args))
        .collect();
    let order = match setting {
        ClipboardHistory::None => return None,
        ClipboardHistory::Auto => vec![
            ClipboardHistory::Cliphist,
            ClipboardHistory::Copyq,
            ClipboardHistory::Clipman,
        ],
        explicit => vec![explicit],
    };
    order
        .iter()
        .find_map(|kind| {
            running
                .iter()
                .find(|backend| backend.kind() == *kind)
                .cloned()
        })
        .or_else(|| match setting {
            ClipboardHistory::Cliphist => Some(Backend::Cliphist { flags: Vec::new() }),
            ClipboardHistory::Copyq => Some(Backend::CopyQ),
            ClipboardHistory::Clipman => Some(Backend::Clipman {
                path: PathBuf::from(crate::expand_tilde(CLIPMAN_DEFAULT_PATH)),
            }),
            _ => None,
        })
}

/// Recognise one clipboard manager process, preserving argument boundaries.
/// Only a shell's `-c` command needs tokenizing to recover quoted paths.
fn parse_cmdline(args: &[String]) -> Option<Backend> {
    let shell_command;
    let args = if matches!(
        args.first().map(|arg| basename(arg)),
        Some("sh" | "bash" | "dash" | "zsh" | "ksh" | "ash")
    ) {
        let option = args
            .iter()
            .position(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c'))?;
        shell_command = shlex::split(args.get(option + 1)?)?;
        &shell_command
    } else {
        args
    };
    if args.first().map(|arg| basename(arg)) == Some("copyq") {
        return Some(Backend::CopyQ);
    }
    let tokens: Vec<&str> = args.iter().map(String::as_str).collect();
    for (position, token) in tokens.iter().enumerate() {
        let rest = &tokens[position + 1..];
        if !rest.contains(&"store") {
            continue;
        }
        match basename(token) {
            "cliphist" => {
                let flags = ["db-path", "config-path"]
                    .iter()
                    .filter_map(|name| {
                        flag_value(rest, name).map(|value| [format!("-{name}"), value])
                    })
                    .flatten()
                    .collect();
                return Some(Backend::Cliphist { flags });
            }
            "clipman" => {
                let path = flag_value(rest, "histpath")
                    .unwrap_or_else(|| CLIPMAN_DEFAULT_PATH.to_string());
                return Some(Backend::Clipman {
                    path: PathBuf::from(crate::expand_tilde(&path)),
                });
            }
            _ => {}
        }
    }
    None
}

/// Value of `-name`/`--name`, given as `--name=value` or `--name value`.
fn flag_value(tokens: &[&str], name: &str) -> Option<String> {
    tokens.iter().enumerate().find_map(|(position, token)| {
        let flag = token
            .strip_prefix("--")
            .or_else(|| token.strip_prefix('-'))?;
        if flag == name {
            return tokens.get(position + 1).map(|value| value.to_string());
        }
        flag.strip_prefix(name)?
            .strip_prefix('=')
            .map(str::to_string)
    })
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn test_parse_cmdline_recognises_managers() {
        assert_eq!(
            parse_cmdline(&cmd("wl-paste --watch cliphist store")),
            Some(Backend::Cliphist { flags: Vec::new() })
        );
        assert_eq!(
            parse_cmdline(&[
                "/bin/sh".into(),
                "-c".into(),
                "wl-paste --type text --watch /nix/store/abc-cliphist-0.7/bin/cliphist -db-path /tmp/db store".into(),
            ]),
            Some(Backend::Cliphist {
                flags: vec!["-db-path".into(), "/tmp/db".into()]
            })
        );
        assert_eq!(
            parse_cmdline(&cmd(
                "wl-paste --watch cliphist --config-path=/c/conf store"
            )),
            Some(Backend::Cliphist {
                flags: vec!["-config-path".into(), "/c/conf".into()]
            })
        );
        assert_eq!(
            parse_cmdline(&cmd(
                "wl-paste -t text --watch clipman store --histpath=/h.json"
            )),
            Some(Backend::Clipman {
                path: "/h.json".into()
            })
        );
        assert_eq!(
            parse_cmdline(&cmd("wl-paste --watch clipman --histpath /x.json store")),
            Some(Backend::Clipman {
                path: "/x.json".into()
            })
        );
        assert_eq!(
            parse_cmdline(&cmd("/usr/bin/copyq --clipboard-access monitorClipboard")),
            Some(Backend::CopyQ)
        );
        assert_eq!(parse_cmdline(&cmd("cliphist list")), None);
        assert_eq!(parse_cmdline(&cmd("vim clipman.json")), None);
        assert_eq!(parse_cmdline(&cmd("myclipman store")), None);
    }

    #[test]
    fn test_parse_cmdline_preserves_path_arguments() {
        for equals in [false, true] {
            let mut args = cmd("wl-paste --watch cliphist");
            let mut flags = Vec::new();
            for (name, value) in [
                ("db-path", "/tmp/clip history/db"),
                ("config-path", "/tmp/clip history/it's \"config\""),
            ] {
                if equals {
                    args.push(format!("--{name}={value}"));
                } else {
                    args.extend([format!("-{name}"), value.to_string()]);
                }
                flags.extend([format!("-{name}"), value.to_string()]);
            }
            args.push("store".into());
            assert_eq!(parse_cmdline(&args), Some(Backend::Cliphist { flags }));

            let mut args = cmd("wl-paste --watch clipman store");
            let path = "/tmp/clip history/it's \"history\".json";
            if equals {
                args.push(format!("--histpath={path}"));
            } else {
                args.extend(["--histpath".into(), path.into()]);
            }
            assert_eq!(
                parse_cmdline(&args),
                Some(Backend::Clipman { path: path.into() })
            );
        }
    }

    #[test]
    fn test_parse_cmdline_tokenizes_only_shell_commands() {
        for command in [
            "wl-paste --watch cliphist -db-path '/tmp/clip history/db' store",
            "wl-paste --watch cliphist --db-path=\"/tmp/clip history/db\" store",
            r"wl-paste --watch cliphist -db-path /tmp/clip\ history/db store",
        ] {
            assert_eq!(
                parse_cmdline(&["/bin/sh".into(), "-c".into(), command.into()]),
                Some(Backend::Cliphist {
                    flags: vec!["-db-path".into(), "/tmp/clip history/db".into()]
                })
            );
        }
        assert_eq!(
            parse_cmdline(&[
                "bash".into(),
                "--norc".into(),
                "-lc".into(),
                "wl-paste --watch clipman store --histpath '/tmp/clip history/h.json'".into(),
            ]),
            Some(Backend::Clipman {
                path: "/tmp/clip history/h.json".into()
            })
        );
        assert_eq!(
            parse_cmdline(&["printf".into(), "cliphist store".into()]),
            None
        );
    }

    #[test]
    fn test_detect_from_cmdlines_order_and_override() {
        let running = vec![
            cmd("wl-paste --watch clipman store --histpath=/h.json"),
            cmd("copyq"),
            cmd("bash"),
        ];
        assert_eq!(
            detect_from_cmdlines(&running, ClipboardHistory::Auto),
            Some(Backend::CopyQ)
        );
        assert_eq!(
            detect_from_cmdlines(&running, ClipboardHistory::Clipman),
            Some(Backend::Clipman {
                path: "/h.json".into()
            })
        );
        assert_eq!(
            detect_from_cmdlines(&running, ClipboardHistory::Cliphist),
            Some(Backend::Cliphist { flags: Vec::new() })
        );
        assert_eq!(detect_from_cmdlines(&running, ClipboardHistory::None), None);
        assert_eq!(
            detect_from_cmdlines(&[cmd("bash")], ClipboardHistory::Auto),
            None
        );
        assert!(matches!(
            detect_from_cmdlines(&[cmd("bash")], ClipboardHistory::Clipman),
            Some(Backend::Clipman { path }) if path.ends_with(".local/share/clipman.json")
        ));
    }

    #[test]
    fn test_history_parsers() {
        let list = "30\tnewest\n29\t[[ binary data 12 KiB png 10x10 ]]\n28\tolder\n";
        assert_eq!(cliphist_id(list, 0), Ok("30"));
        assert_eq!(cliphist_id(list, 2), Ok("28"));
        assert!(cliphist_id(list, 1).unwrap_err().contains("not text"));
        assert!(cliphist_id(list, 3).unwrap_err().contains("only 3"));

        let json = r#"["oldest", "middle", "newest"]"#;
        assert_eq!(clipman_entry(json, 0).unwrap(), "newest");
        assert_eq!(clipman_entry(json, 2).unwrap(), "oldest");
        assert!(clipman_entry(json, 3).unwrap_err().contains("only 3"));
        assert!(clipman_entry("{", 1).is_err());

        assert_eq!(parse_copyq_count(b"1000\n"), Ok(1000));
        assert!(parse_copyq_count(b"nope").is_err());
        assert!(text_entry(Vec::new(), 4).unwrap_err().contains("not text"));
        assert!(text_entry(vec![0xff], 4).is_err());
    }

    #[test]
    fn test_read_entries_from_clipman_file() {
        let path = std::env::temp_dir().join(format!("raffi-clipman-{}.json", std::process::id()));
        fs::write(&path, r#"["a", "b", "c"]"#).unwrap();
        let entries = read_entries(&Backend::Clipman { path: path.clone() }, &[1, 5]);
        assert_eq!(entries[&1], Ok("b".to_string()));
        assert!(entries[&5].is_err());
        let _ = fs::remove_file(&path);

        let entries = read_entries(&Backend::Clipman { path }, &[1]);
        assert!(entries[&1].as_ref().unwrap_err().contains("cannot read"));
    }
}
