use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::placeholders::MAX_VALUE;

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);
/// Typing a key per character is slow and each press is a command argument, so
/// keep caret movement for `{cursor}` within a sane bound.
const MAX_CURSOR_MOVES: usize = 1000;

fn find_tool(candidates: &[&'static str]) -> Option<&'static str> {
    std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            for candidate in candidates {
                if dir.join(candidate).is_file() {
                    return Some(*candidate);
                }
            }
        }
        None
    })
}

/// Shell command that types `$RAFFI_INSERT_VALUE` and then presses Left
/// `cursor_moves` times. wtype treats every argument after `--` as text, so
/// the key presses go through a second invocation.
fn insert_command(tool: &str, cursor_moves: usize) -> String {
    let mut command = if tool == "ydotool" {
        "ydotool type -- \"$RAFFI_INSERT_VALUE\"".to_string()
    } else {
        "wtype -- \"$RAFFI_INSERT_VALUE\"".to_string()
    };
    if cursor_moves > 0 {
        if tool == "ydotool" {
            command.push_str(" && ydotool key");
            // 105 is KEY_LEFT; each move is a press followed by a release.
            command.push_str(&" 105:1 105:0".repeat(cursor_moves));
        } else {
            command.push_str(" && wtype");
            command.push_str(&" -k Left".repeat(cursor_moves));
        }
    }
    format!("sleep 0.2 && {command}")
}

fn spawn_insert(value: &str, cursor_moves: usize) -> Result<(), String> {
    let tool =
        find_tool(&["wtype", "ydotool"]).ok_or("cannot type text: install wtype or ydotool")?;
    Command::new("sh")
        .arg("-c")
        .arg(insert_command(tool, cursor_moves))
        .env("RAFFI_INSERT_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot run {tool}: {error}"))
}

/// Copy a value to the clipboard using the first available tool.
/// Fallback chain: wl-copy -> xclip -> xsel.
pub(super) fn spawn_copy(value: &str) -> bool {
    try_copy(value).is_ok()
}

fn try_copy(value: &str) -> Result<(), String> {
    let tool = find_tool(&["wl-copy", "xclip", "xsel"])
        .ok_or("cannot copy: install wl-clipboard, xclip or xsel")?;

    let copy_cmd = match tool {
        "xclip" => "printf '%s' \"$RAFFI_COPY_VALUE\" | xclip -selection clipboard".to_string(),
        "xsel" => "printf '%s' \"$RAFFI_COPY_VALUE\" | xsel --clipboard --input".to_string(),
        _ => "wl-copy -- \"$RAFFI_COPY_VALUE\"".to_string(),
    };
    Command::new("sh")
        .arg("-c")
        .arg(&copy_cmd)
        .env("RAFFI_COPY_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot run {tool}: {error}"))
}

fn spawn_custom_action(action: &str, value: &str) -> Result<(), String> {
    let command = action.replace("{value}", value);
    crate::debug_log!("action: executing shell command={command:?}");
    Command::new("sh")
        .arg("-c")
        .arg(&command)
        .env("RAFFI_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot run action: {error}"))
}

/// Execute an action for a selected value.
/// Supported keywords:
/// - `insert`: type into the focused app via `wtype` or `ydotool`
/// - `copy`: copy to clipboard via the first available clipboard tool
/// - other: execute as `sh -c` after replacing `{value}` in the command string;
///   the value is also available as `$RAFFI_VALUE`
pub(super) fn execute_action(action: &str, value: &str) {
    crate::debug_log!("action: action={action:?} value={value:?}");
    let _ = dispatch(action, value, 0);
}

fn dispatch(action: &str, value: &str, cursor_moves: usize) -> Result<(), String> {
    match action {
        "insert" => spawn_insert(value, cursor_moves),
        "copy" => try_copy(value),
        _ => spawn_custom_action(action, value),
    }
}

/// Run a snippet action, reporting anything that would stop the value from
/// reaching its destination. `cursor_moves` only applies to `insert`.
pub(super) fn execute_snippet_action(
    action: &str,
    value: &str,
    cursor_moves: Option<usize>,
) -> Result<(), String> {
    crate::debug_log!("action: snippet action={action:?} cursor={cursor_moves:?}");
    if value.len() > MAX_VALUE {
        return Err(format!(
            "snippet is larger than {} KiB and cannot be passed to the action",
            MAX_VALUE / 1024
        ));
    }
    if value.contains('\0') {
        return Err("snippet contains a NUL byte and cannot be passed to the action".into());
    }
    let cursor_moves = if action == "insert" {
        let moves = cursor_moves.unwrap_or(0);
        if moves > MAX_CURSOR_MOVES {
            return Err(format!(
                "{{cursor}} is {moves} characters from the end, the limit is {MAX_CURSOR_MOVES}"
            ));
        }
        moves
    } else {
        0
    };
    dispatch(action, value, cursor_moves)
}

enum ReadOutcome {
    Done(Vec<u8>),
    Overflow,
    Failed(String),
}

fn kill_process_group(pgid: u32) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "kill -s TERM -- -{pgid} 2>/dev/null; sleep 0.1; kill -s KILL -- -{pgid} 2>/dev/null"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run `command` in its own process group and return its stdout.
///
/// The deadline covers both reading output and waiting for exit. On timeout
/// or when stdout exceeds `max_output` bytes, the whole process group is
/// killed so that background children cannot keep the pipe open.
pub(super) fn run_bounded(
    mut command: Command,
    timeout: Duration,
    max_output: usize,
) -> Result<Vec<u8>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start: {error}"))?;
    let pgid = child.id();
    let deadline = Instant::now() + timeout;

    let Some(mut stdout) = child.stdout.take() else {
        kill_process_group(pgid);
        let _ = child.wait();
        return Err("cannot read output".into());
    };
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0u8; 8192];
        let outcome = loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break ReadOutcome::Done(output),
                Ok(count) => {
                    output.extend_from_slice(&chunk[..count]);
                    if output.len() > max_output {
                        break ReadOutcome::Overflow;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => break ReadOutcome::Failed(error.to_string()),
            }
        };
        let _ = sender.send(outcome);
    });

    let fail = |child: &mut std::process::Child, message: String| {
        kill_process_group(pgid);
        let _ = child.wait();
        Err(message)
    };

    let output = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(ReadOutcome::Done(output)) => output,
        Ok(ReadOutcome::Overflow) => {
            return fail(
                &mut child,
                format!("output is larger than {} KiB", max_output / 1024),
            )
        }
        Ok(ReadOutcome::Failed(error)) => {
            return fail(&mut child, format!("cannot read output: {error}"))
        }
        Err(_) => {
            return fail(
                &mut child,
                format!("timed out after {}s", timeout.as_secs_f32()),
            )
        }
    };

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                return fail(
                    &mut child,
                    format!("timed out after {}s", timeout.as_secs_f32()),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return fail(&mut child, format!("cannot wait: {error}")),
        }
    };
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(output)
}

/// Read the clipboard as text, trying wl-paste, xclip and xsel in turn.
pub(super) fn read_clipboard() -> Result<String, String> {
    let backends: [(&str, &[&str]); 3] = [
        ("wl-paste", &["--no-newline", "--type", "text"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    let mut errors = Vec::new();
    for (tool, args) in backends {
        let mut command = Command::new(tool);
        command.args(args);
        match run_bounded(command, CLIPBOARD_TIMEOUT, MAX_VALUE) {
            Ok(bytes) => {
                return String::from_utf8(bytes).map_err(|_| "clipboard is not text".to_string())
            }
            // Tools that are not installed only add noise to the error.
            Err(error) if error.starts_with("cannot start:") => {}
            Err(error) => {
                if tool == "wl-paste" {
                    if let Some(reason) = wayland_clipboard_without_text() {
                        return Err(reason);
                    }
                }
                errors.push(format!("{tool}: {error}"));
            }
        }
    }
    if errors.is_empty() {
        return Err("no clipboard tool found: install wl-clipboard, xclip or xsel".into());
    }
    Err(format!(
        "clipboard is empty or unavailable ({})",
        errors.join("; ")
    ))
}

/// Explain a failed `wl-paste --type text` when the clipboard is empty or
/// holds only non-text data such as an image.
fn wayland_clipboard_without_text() -> Option<String> {
    let mut command = Command::new("wl-paste");
    command.arg("--list-types");
    match run_bounded(command, CLIPBOARD_TIMEOUT, 64 * 1024) {
        Ok(output) => describe_non_text(&String::from_utf8_lossy(&output)),
        Err(_) => None,
    }
}

fn describe_non_text(types: &str) -> Option<String> {
    let types: Vec<&str> = types
        .lines()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .collect();
    if types.is_empty() {
        return Some("clipboard is empty".into());
    }
    let is_text = |kind: &&str| {
        kind.starts_with("text/") || matches!(*kind, "TEXT" | "STRING" | "UTF8_STRING")
    };
    if types.iter().any(is_text) {
        return None;
    }
    let shown: Vec<&str> = types.iter().copied().take(3).collect();
    Some(format!(
        "clipboard has no text (it holds {})",
        shown.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn test_insert_command_without_cursor() {
        assert_eq!(
            insert_command("wtype", 0),
            "sleep 0.2 && wtype -- \"$RAFFI_INSERT_VALUE\""
        );
        assert_eq!(
            insert_command("ydotool", 0),
            "sleep 0.2 && ydotool type -- \"$RAFFI_INSERT_VALUE\""
        );
    }

    #[test]
    fn test_insert_command_moves_cursor_in_a_separate_invocation() {
        assert_eq!(
            insert_command("wtype", 2),
            "sleep 0.2 && wtype -- \"$RAFFI_INSERT_VALUE\" && wtype -k Left -k Left"
        );
        assert_eq!(
            insert_command("ydotool", 2),
            "sleep 0.2 && ydotool type -- \"$RAFFI_INSERT_VALUE\" && ydotool key 105:1 105:0 105:1 105:0"
        );
    }

    #[test]
    fn test_snippet_action_rejects_values_that_cannot_be_passed() {
        let big = "x".repeat(MAX_VALUE + 1);
        assert!(execute_snippet_action("copy", &big, None)
            .unwrap_err()
            .contains("larger than"));
        assert!(execute_snippet_action("copy", "a\0b", None)
            .unwrap_err()
            .contains("NUL"));
        assert!(
            execute_snippet_action("insert", "abc", Some(MAX_CURSOR_MOVES + 1))
                .unwrap_err()
                .contains("{cursor}")
        );
    }

    #[test]
    fn test_custom_action_receives_hostile_value_through_env() {
        let dir = std::env::temp_dir().join(format!("raffi-action-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let value = "it's \"quoted\"\n$(touch pwned) `touch pwned2` ; echo hi";
        // Run inside `dir` so an injected `touch pwned` would land there.
        let action = format!(
            "cd '{}' && printf '%s' \"$RAFFI_VALUE\" > '{}'",
            dir.display(),
            out.display()
        );
        execute_snippet_action(&action, value, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&out).is_ok_and(|written| written == value) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), value);
        assert!(!dir.join("pwned").exists());
        assert!(!dir.join("pwned2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_bounded_returns_output() {
        let output = run_bounded(sh("printf 'hi\\n'"), Duration::from_secs(5), 1024).unwrap();
        assert_eq!(output, b"hi\n");
    }

    #[test]
    fn test_run_bounded_reports_nonzero_exit() {
        let error = run_bounded(sh("echo out; exit 3"), Duration::from_secs(5), 1024).unwrap_err();
        assert!(error.contains("exited"), "{error}");
    }

    #[test]
    fn test_run_bounded_times_out() {
        let started = Instant::now();
        let error = run_bounded(sh("sleep 10"), Duration::from_millis(200), 1024).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_kills_processes_ignoring_term() {
        let started = Instant::now();
        let error = run_bounded(
            sh("trap '' TERM; sleep 3 & wait"),
            Duration::from_millis(200),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_run_bounded_kills_on_output_overflow() {
        let started = Instant::now();
        let error = run_bounded(sh("yes"), Duration::from_secs(10), 4096).unwrap_err();
        assert!(error.contains("larger than"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_kills_descendants_holding_stdout() {
        let started = Instant::now();
        let error = run_bounded(
            sh("(sleep 10 &); echo parent done"),
            Duration::from_millis(300),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn test_run_bounded_reports_missing_program() {
        let error = run_bounded(
            Command::new("raffi-no-such-program"),
            Duration::from_secs(1),
            1024,
        )
        .unwrap_err();
        assert!(error.contains("cannot start"), "{error}");
    }

    #[test]
    fn test_describe_non_text_clipboard() {
        assert_eq!(describe_non_text("").as_deref(), Some("clipboard is empty"));
        assert_eq!(
            describe_non_text("image/png\nimage/jpeg\n").as_deref(),
            Some("clipboard has no text (it holds image/png, image/jpeg)")
        );
        assert_eq!(describe_non_text("image/png\ntext/plain\n"), None);
        assert_eq!(describe_non_text("UTF8_STRING\n"), None);
    }
}
