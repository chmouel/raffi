use std::process::{Command, Stdio};

fn spawn_insert(value: &str) -> bool {
    let tool = std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            if dir.join("wtype").is_file() {
                return Some("wtype");
            }
            if dir.join("ydotool").is_file() {
                return Some("ydotool");
            }
        }
        None
    });
    let Some(tool) = tool else {
        return false;
    };

    let insert_cmd = if tool == "ydotool" {
        "ydotool type -- \"$RAFFI_INSERT_VALUE\"".to_string()
    } else {
        "wtype -- \"$RAFFI_INSERT_VALUE\"".to_string()
    };
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("sleep 0.2 && {insert_cmd}"))
        .env("RAFFI_INSERT_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    true
}

/// Copy a value to the clipboard using the first available tool.
/// Fallback chain: wl-copy -> xclip -> xsel.
pub(super) fn spawn_copy(value: &str) -> bool {
    let tool = std::env::var_os("PATH").and_then(|paths| {
        for dir in std::env::split_paths(&paths) {
            if dir.join("wl-copy").is_file() {
                return Some("wl-copy");
            }
            if dir.join("xclip").is_file() {
                return Some("xclip");
            }
            if dir.join("xsel").is_file() {
                return Some("xsel");
            }
        }
        None
    });
    let Some(tool) = tool else {
        return false;
    };

    let copy_cmd = match tool {
        "xclip" => "printf '%s' \"$RAFFI_COPY_VALUE\" | xclip -selection clipboard".to_string(),
        "xsel" => "printf '%s' \"$RAFFI_COPY_VALUE\" | xsel --clipboard --input".to_string(),
        _ => "wl-copy -- \"$RAFFI_COPY_VALUE\"".to_string(),
    };
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&copy_cmd)
        .env("RAFFI_COPY_VALUE", value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    true
}

/// Execute an action for a selected value.
/// Supported keywords:
/// - `insert`: type into the focused app via `wtype` or `ydotool`
/// - `copy`: copy to clipboard via the first available clipboard tool
/// - other: execute as `sh -c` after replacing `{value}` in the command string
pub(super) fn execute_action(action: &str, value: &str) {
    match action {
        "insert" => {
            spawn_insert(value);
        }
        "copy" => {
            spawn_copy(value);
        }
        _ => {
            let command = action.replace("{value}", value);
            let _ = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}
