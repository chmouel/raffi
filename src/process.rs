//! Starting child processes without the environment Raffi set for itself,
//! and reading the environment of the client a served window is for.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::env::VarError;
use std::ffi::{OsStr, OsString};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The environment of a client whose window the server shows, tagged with a
/// unique scope id so background work stays tied to the window that started it.
#[derive(Clone)]
pub(crate) struct ClientEnv {
    vars: Arc<HashMap<String, String>>,
    scope: u64,
}

#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

impl ClientEnv {
    #[cfg_attr(not(feature = "wayland"), allow(dead_code))]
    pub(crate) fn new(vars: HashMap<String, String>) -> Self {
        Self {
            vars: Arc::new(vars),
            scope: NEXT_SCOPE.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// The environment of the window being served, read by every thread while it
/// runs and finishes.
static SERVED_ENV: Mutex<Option<ClientEnv>> = Mutex::new(None);

thread_local! {
    /// Overrides `SERVED_ENV` on one thread, while it loads a client's
    /// session next to a window still showing for another.
    static THREAD_ENV: RefCell<Option<ClientEnv>> = const { RefCell::new(None) };
    /// Set on the server's own threads, which keep its environment while a
    /// window shows.
    static SERVER_THREAD: Cell<bool> = const { Cell::new(false) };
}

fn served_env() -> Option<ClientEnv> {
    if let Some(env) = THREAD_ENV.with(|env| env.borrow().clone()) {
        return Some(env);
    }
    if SERVER_THREAD.with(Cell::get) {
        return None;
    }
    SERVED_ENV.lock().ok().and_then(|env| env.clone())
}

/// Keep the server's own environment on this thread, for the server's
/// background work. Threads started through [`inherit`] keep it too.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn mark_server_thread() {
    SERVER_THREAD.with(|server| server.set(true));
}

/// The environment work started now has to keep: the served caller's, or
/// `None` for the server's own.
#[derive(Clone)]
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) struct Context(Option<ClientEnv>);

#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn context() -> Context {
    Context(served_env())
}

#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
impl Context {
    /// Run `f` in this environment on the current thread.
    pub(crate) fn enter<T>(&self, f: impl FnOnce() -> T) -> T {
        match &self.0 {
            Some(env) => with_env(env.clone(), f),
            None => {
                let previous = SERVER_THREAD.with(|server| server.replace(true));
                let result = f();
                SERVER_THREAD.with(|server| server.set(previous));
                result
            }
        }
    }
}

/// Wrap `f`, to run on a new thread, so it keeps the environment current
/// here even after the window it was started for has closed and another
/// caller's is served.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn inherit<T>(f: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let context = context();
    move || context.enter(f)
}

/// Identifies the served window this thread is working for, or `None` for
/// the process's own environment.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn env_scope() -> Option<u64> {
    served_env().map(|env| env.scope)
}

/// Make `env` the environment Raffi reads and gives its children, until
/// cleared with `None`.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn set_served_env(env: Option<ClientEnv>) {
    if let Ok(mut served) = SERVED_ENV.lock() {
        *served = env;
    }
}

/// Run `f` with `env` as the environment on this thread.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub(crate) fn with_env<T>(env: ClientEnv, f: impl FnOnce() -> T) -> T {
    let previous = THREAD_ENV.with(|current| current.replace(Some(env)));
    let result = f();
    THREAD_ENV.with(|current| *current.borrow_mut() = previous);
    result
}

/// `std::env::var_os`, from the client's environment while serving a window.
/// Use it for every variable Raffi reads.
pub(crate) fn var_os(name: impl AsRef<str>) -> Option<OsString> {
    let name = name.as_ref();
    match served_env() {
        Some(env) => env.vars.get(name).map(OsString::from),
        None => std::env::var_os(name),
    }
}

/// `std::env::var`, from the client's environment while serving a window.
pub(crate) fn var(name: impl AsRef<str>) -> Result<String, VarError> {
    match var_os(name) {
        Some(value) => value.into_string().map_err(VarError::NotUnicode),
        None => Err(VarError::NotPresent),
    }
}

/// Variables Raffi exported for its own window, which children must not
/// inherit.
static EXPORTED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// A `Command` for `program` that leaves out the variables Raffi exported,
/// and gets the client's environment while serving a window. Use it for
/// every child Raffi starts.
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    if let Some(env) = served_env() {
        command.env_clear().envs(env.vars.iter());
    } else if let Ok(exported) = EXPORTED.lock() {
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

    #[test]
    fn served_environment_is_read_and_given_to_children() {
        let env = ClientEnv::new(HashMap::from([(
            "RAFFI_TEST_CLIENT_VARIABLE".to_string(),
            "client".to_string(),
        )]));
        let (read, printed, path) = with_env(env, || {
            let output = command("/bin/sh")
                .args(["-c", "printf %s \"$RAFFI_TEST_CLIENT_VARIABLE${PATH-}\""])
                .output()
                .unwrap();
            (
                var("RAFFI_TEST_CLIENT_VARIABLE"),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                var_os("PATH"),
            )
        });
        assert_eq!(read.as_deref(), Ok("client"));
        // Only the client's variables, none of this process's.
        assert!(printed.starts_with("client"), "{printed}");
        assert!(path.is_none());
        assert!(var("RAFFI_TEST_CLIENT_VARIABLE").is_err());
    }

    #[test]
    fn server_threads_still_honour_an_explicit_environment() {
        let env = ClientEnv::new(HashMap::from([(
            "RAFFI_TEST_CLIENT_VARIABLE".to_string(),
            "client".to_string(),
        )]));
        std::thread::spawn(move || {
            mark_server_thread();
            assert!(env_scope().is_none());
            let (read, scope) = with_env(env, || (var("RAFFI_TEST_CLIENT_VARIABLE"), env_scope()));
            assert_eq!(read.as_deref(), Ok("client"));
            assert!(scope.is_some());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn started_work_keeps_the_environment_it_was_started_in() {
        let env = ClientEnv::new(HashMap::from([(
            "RAFFI_TEST_CLIENT_VARIABLE".to_string(),
            "client".to_string(),
        )]));
        let for_client = with_env(env, || inherit(|| var("RAFFI_TEST_CLIENT_VARIABLE")));
        let read = std::thread::spawn(for_client).join().unwrap();
        assert_eq!(read.as_deref(), Ok("client"));
        let server = std::thread::spawn(inherit(|| SERVER_THREAD.with(Cell::get)));
        assert!(server.join().unwrap());
    }

    #[test]
    fn each_served_window_gets_a_distinct_scope() {
        let first = ClientEnv::new(HashMap::from([("HOME".to_string(), "/home/u".to_string())]));
        let second = ClientEnv::new(HashMap::from([("HOME".to_string(), "/home/u".to_string())]));
        assert_ne!(with_env(first, env_scope), with_env(second, env_scope));
    }
}
