//! Starting child processes without the environment Raffi set for itself.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Variables Raffi exported for its own window, which children must not
/// inherit.
static EXPORTED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// A `Command` for `program` that leaves out the variables Raffi exported.
/// Use it for every child Raffi starts.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    if let Ok(exported) = EXPORTED.lock() {
        for name in exported.iter() {
            command.env_remove(name);
        }
    }
    command
}

/// Set while the server shows the launcher window for clients.
static HOSTING_WINDOW: AtomicBool = AtomicBool::new(false);

/// Mark this process as the server showing the launcher window.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn set_hosting_window() {
    HOSTING_WINDOW.store(true, Ordering::SeqCst);
}

/// Whether this process is the server showing the launcher window.
pub(crate) fn hosting_window() -> bool {
    HOSTING_WINDOW.load(Ordering::SeqCst)
}

/// Give a program Raffi starts and leaves running no streams when the server
/// shows the window, as a launcher started from a keybinding has none, so
/// its output stays out of the server's log.
pub(crate) fn detach_streams(command: &mut Command) {
    if hosting_window() {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
}

/// Export `name` for Raffi's own use, unless the user already set it.
/// Returns whether it was exported. Setting the environment is not thread
/// safe: call it before other threads start.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn export(name: &'static str, value: &str) -> bool {
    if std::env::var_os(name).is_some() {
        return false;
    }
    std::env::set_var(name, value);
    if let Ok(mut exported) = EXPORTED.lock() {
        exported.push(name);
    }
    true
}

/// Take back every variable `export` set.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn unexport_all() {
    if let Ok(mut exported) = EXPORTED.lock() {
        for name in exported.drain(..) {
            std::env::remove_var(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_do_not_inherit_exported_variables() {
        let name = "RAFFI_TEST_EXPORTED_VARIABLE";
        assert!(export(name, "1"));
        assert!(!export(name, "2"));
        let output = command("sh")
            .args(["-c", "printf %s \"${RAFFI_TEST_EXPORTED_VARIABLE-unset}\""])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "unset");
        unexport_all();
        assert!(std::env::var_os(name).is_none());
    }
}
