//! Browser profile data shared by the bookmarks and browser history addons:
//! opening links in the browser they come from, and reading live SQLite
//! databases safely.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use crate::{BookmarkBrowser, BookmarkOpener};

use super::actions::DeferredAction;

pub(super) async fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.ok()
}

/// What opens a source's links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Opener {
    /// Run this, with `{url}` replaced by the link; `name` says by what.
    /// The window of one of `apps` is focused after.
    Argv {
        argv: Vec<String>,
        name: String,
        apps: Vec<String>,
    },
    /// Why the links cannot be opened.
    Missing(String),
}

impl Opener {
    pub(super) fn action(&self, url: &str) -> Option<DeferredAction> {
        let argv = self.command(url)?;
        Some(match self {
            Self::Argv { apps, .. } if !apps.is_empty() => DeferredAction::OpenInApp {
                argv,
                apps: apps.clone(),
            },
            _ => DeferredAction::Command(argv),
        })
    }

    pub(super) fn command(&self, url: &str) -> Option<Vec<String>> {
        match self {
            Self::Argv { argv, .. } => {
                Some(argv.iter().map(|arg| arg.replace("{url}", url)).collect())
            }
            Self::Missing(_) => None,
        }
    }
}

pub(super) fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What opens the links of a source of `browser` read from `profile`.
pub(super) fn opener(
    choice: Option<&BookmarkOpener>,
    browser: Option<BookmarkBrowser>,
    profile: Option<&Path>,
    path: &OsStr,
) -> Opener {
    let fallback = || Opener::Argv {
        argv: vec!["xdg-open".to_string(), "{url}".to_string()],
        name: "the default browser".to_string(),
        apps: Vec::new(),
    };
    match choice {
        None => match browser {
            Some(browser) => match browser_opener(browser, Some(browser), profile, path) {
                // A browser installed some other way, such as a Flatpak.
                Opener::Missing(_) => fallback(),
                opener => opener,
            },
            None => fallback(),
        },
        Some(BookmarkOpener::Browser(target)) => browser_opener(*target, browser, profile, path),
        Some(BookmarkOpener::Command(argv)) => {
            let Some(program) = argv.first().filter(|program| !program.is_empty()) else {
                return Opener::Missing("open_with is an empty command".to_string());
            };
            if crate::browser_host::find_program(&[program.as_str()], path).is_none() {
                return Opener::Missing(format!("{program} is not installed"));
            }
            let mut argv = argv.clone();
            if !argv.iter().any(|arg| arg.contains("{url}")) {
                argv.push("{url}".to_string());
            }
            let name = dir_name(Path::new(program));
            Opener::Argv {
                argv,
                apps: vec![name.clone()],
                name,
            }
        }
    }
}

/// Open in `target`, in the profile the data comes from when it is their
/// browser.
fn browser_opener(
    target: BookmarkBrowser,
    source: Option<BookmarkBrowser>,
    profile: Option<&Path>,
    path: &OsStr,
) -> Opener {
    let Some((name, _, gecko)) = crate::browser_host::browser_details(target) else {
        return Opener::Missing("unknown browser".to_string());
    };
    let Some(program) = crate::browser_host::browser_program(target, path) else {
        return Opener::Missing(format!("{name} is not installed"));
    };
    let mut argv = vec![program.to_string_lossy().into_owned()];
    if let Some(profile) = profile.filter(|_| source == Some(target)) {
        if gecko {
            argv.extend([
                "--profile".to_string(),
                profile.to_string_lossy().into_owned(),
            ]);
        } else if profile
            .parent()
            .is_some_and(|data| data.join("Local State").is_file())
        {
            argv.push(format!("--profile-directory={}", dir_name(profile)));
        }
    }
    argv.push("{url}".to_string());
    Opener::Argv {
        argv,
        name: name.to_string(),
        apps: crate::browser_host::browser_window_apps(target),
    }
}

/// A private copy of a live database and its sidecar files, removed when
/// dropped.
pub(super) struct DatabaseSnapshot {
    dir: PathBuf,
}

impl Drop for DatabaseSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Copy `file` and its SQLite sidecars into a private temporary directory.
/// Blocking: run it off the UI thread.
pub(super) fn snapshot_database(
    file: &Path,
    prefix: &str,
) -> Result<(DatabaseSnapshot, PathBuf), String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "raffi-{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let snapshot = DatabaseSnapshot { dir };
    let name = file
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("database"));
    let target = snapshot.dir.join(&name);
    fs::copy(file, &target).map_err(|error| format!("cannot copy {}: {error}", file.display()))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = file.as_os_str().to_owned();
        sidecar.push(suffix);
        let sidecar = Path::new(&sidecar);
        if sidecar.is_file() {
            let mut copied = name.clone();
            copied.push(suffix);
            let _ = fs::copy(sidecar, snapshot.dir.join(copied));
        }
    }
    Ok((snapshot, target))
}
