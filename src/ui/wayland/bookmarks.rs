//! Browser bookmarks: read from Firefox-style `places.sqlite` and Chromium
//! `Bookmarks` files, browsed by folder or searched across them.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::browser_host::{self, BrowserProfile};
use crate::{BookmarkBrowser, BookmarkOpener, BookmarksAddonConfig};

use super::actions::DeferredAction;
use super::command_list::{domains_of, CommandAction, CommandItem};
use super::url_match::uri_host;

const FOLDER_ICON: &str = "folder";
const BOOKMARK_ICONS: [&str; 2] = ["user-bookmarks", "web-browser"];
/// Shown for a `/` in a folder name, which separates folders in the query.
const SLASH: char = '\u{2215}';
const UNTITLED: &str = "Untitled";
const SEARCH_BADGE: &str = "Search";
const MAX_DEPTH: usize = 64;
/// Microseconds between 1601-01-01, Chromium's epoch, and 1970-01-01.
const CHROMIUM_EPOCH_OFFSET: i64 = 11_644_473_600_000_000;
const GECKO_ROOTS: [(&str, &str); 4] = [
    ("toolbar_____", "Bookmarks Toolbar"),
    ("menu________", "Bookmarks Menu"),
    ("unfiled_____", "Other Bookmarks"),
    ("mobile______", "Mobile Bookmarks"),
];
const CHROMIUM_ROOTS: [(&str, &str); 3] = [
    ("bookmark_bar", "Bookmarks bar"),
    ("other", "Other bookmarks"),
    ("synced", "Mobile bookmarks"),
];

pub(super) const IDENTITY_PREFIX: &str = "bookmark:";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Folder {
    pub name: String,
    pub folders: Vec<Folder>,
    pub bookmarks: Vec<Bookmark>,
}

impl Folder {
    fn count(&self) -> usize {
        self.bookmarks.len() + self.folders.iter().map(Folder::count).sum::<usize>()
    }

    fn is_empty(&self) -> bool {
        self.folders.is_empty() && self.bookmarks.is_empty()
    }

    /// The folder at `segments`, matched without case, and the names of the
    /// folders on the way as typed in a query.
    fn walk<'a>(&'a self, segments: &[&str]) -> Option<(&'a Folder, Vec<String>)> {
        let mut folder = self;
        let mut names = Vec::new();
        for wanted in segments {
            folder = folder
                .folders
                .iter()
                .find(|sub| segment(&sub.name).to_lowercase() == wanted.to_lowercase())?;
            names.push(segment(&folder.name));
        }
        Some((folder, names))
    }

    fn hosts(&self, out: &mut BTreeSet<String>) {
        out.extend(
            self.bookmarks
                .iter()
                .filter_map(|bookmark| super::favicons::host_key(&bookmark.url)),
        );
        for folder in &self.folders {
            folder.hosts(out);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Bookmark {
    pub title: String,
    pub url: String,
    /// Stable id from the browser, part of the row identity.
    pub id: String,
    /// Unix seconds.
    pub added: Option<i64>,
    /// Firefox keyword typed before a search term to search with it.
    pub keyword: Option<String>,
}

/// What opens a source's links.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    fn action(&self, url: &str) -> Option<DeferredAction> {
        let argv = self.command(url)?;
        Some(match self {
            Self::Argv { apps, .. } if !apps.is_empty() => DeferredAction::OpenInApp {
                argv,
                apps: apps.clone(),
            },
            _ => DeferredAction::Command(argv),
        })
    }

    fn command(&self, url: &str) -> Option<Vec<String>> {
        match self {
            Self::Argv { argv, .. } => {
                Some(argv.iter().map(|arg| arg.replace("{url}", url)).collect())
            }
            Self::Missing(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Source {
    /// Stable id for row identities.
    pub key: String,
    pub label: String,
    pub icon: String,
    /// The listed folders and bookmarks: the browser's roots, or the
    /// configured `folder`.
    pub root: Folder,
    pub opener: Opener,
    /// Favicon databases, `true` marking Firefox-style ones.
    pub favicons: Vec<(PathBuf, bool)>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Library {
    pub sources: Vec<Source>,
    /// Sources that could not be read.
    pub errors: Vec<String>,
    /// Site icons by [`super::favicons::host_key`].
    pub icons: HashMap<String, PathBuf>,
}

/// The bookmarks mode: the loaded library and what the rows show.
#[derive(Debug, Default)]
pub(super) struct BookmarksState {
    /// The command list session the state belongs to.
    pub session: u64,
    /// The query after the keyword the rows show.
    pub query: String,
    pub library: Option<Arc<Library>>,
    /// The source Ctrl+Tab shows alone; every source when None.
    pub view: Option<usize>,
    /// Query path each folder row opens, by identity.
    pub folders: HashMap<String, String>,
    /// The folder path, source view and whether a filter was typed that the
    /// rows were built for.
    pub built: Option<(String, Option<usize>, bool)>,
    /// Asking for the term of a search bookmark.
    pub prompt: Option<SearchPrompt>,
}

impl BookmarksState {
    /// The query path that opens the folder row `identity`.
    pub fn folder_path(&self, identity: &str) -> Option<&str> {
        self.folders.get(identity).map(String::as_str)
    }

    /// The label of the source view after `view` going forward or back:
    /// all sources, then each source.
    pub fn next_view(&self, backwards: bool) -> Option<usize> {
        let count = self
            .library
            .as_ref()
            .map_or(0, |library| library.sources.len());
        if count < 2 {
            return None;
        }
        // Positions 0 (all) to count.
        let position = self.view.map_or(0, |view| view + 1);
        let next = if backwards {
            (position + count) % (count + 1)
        } else {
            (position + 1) % (count + 1)
        };
        next.checked_sub(1)
    }

    /// Footer label of the current source view.
    pub fn view_label(&self) -> Option<String> {
        let library = self.library.as_ref()?;
        if library.sources.len() < 2 {
            return None;
        }
        Some(
            self.view
                .and_then(|view| library.sources.get(view))
                .map_or_else(
                    || "All bookmarks".to_string(),
                    |source| source.label.clone(),
                ),
        )
    }
}

/// The folder path and the filter of a query: `Toolbar/Work/rust` is in
/// folder `Toolbar/Work/` and looks for `rust`.
pub(super) fn split_query(query: &str) -> (&str, &str) {
    match query.rfind('/') {
        Some(end) => (&query[..=end], &query[end + 1..]),
        None => ("", query),
    }
}

/// Whether `url` takes a search term, as Firefox search bookmarks do.
pub(super) fn is_search(url: &str) -> bool {
    url.contains("%s") || url.contains("%S")
}

/// `url` with `%s` replaced by the percent-encoded `term` and `%S` by the
/// term as typed, as Firefox does.
pub(super) fn expand(url: &str, term: &str) -> String {
    url.replace("%s", &crate::url_encode_query(term))
        .replace("%S", term)
}

/// The keyword and the term of `text`, a keyword followed by a space.
pub(super) fn keyword_query(text: &str) -> Option<(&str, &str)> {
    let (keyword, term) = text.split_once(' ')?;
    (!keyword.is_empty()).then(|| (keyword, term.trim_start()))
}

/// A search bookmark: what its prompt shows and how it opens.
#[derive(Debug, Clone)]
pub(super) struct SearchTarget {
    /// The identity of the bookmark row.
    pub identity: String,
    pub title: String,
    pub url: String,
    pub source: String,
    pub icon_names: Vec<String>,
    pub opener: Opener,
}

/// The search bookmarks of a library, by row identity and by keyword.
#[derive(Debug, Default)]
pub(super) struct SearchIndex {
    by_identity: HashMap<String, SearchTarget>,
    by_keyword: HashMap<String, String>,
}

impl SearchIndex {
    pub fn new(library: &Library) -> Self {
        fn add(index: &mut SearchIndex, library: &Library, source: &Source, folder: &Folder) {
            for bookmark in folder.bookmarks.iter().filter(|b| is_search(&b.url)) {
                let identity = identity(source, bookmark);
                if let Some(keyword) = &bookmark.keyword {
                    index
                        .by_keyword
                        .entry(keyword.to_lowercase())
                        .or_insert_with(|| identity.clone());
                }
                index.by_identity.insert(
                    identity.clone(),
                    SearchTarget {
                        identity,
                        title: title(bookmark),
                        url: bookmark.url.clone(),
                        source: source.label.clone(),
                        icon_names: icon_names(library, source, &bookmark.url),
                        opener: source.opener.clone(),
                    },
                );
            }
            for sub in &folder.folders {
                add(index, library, source, sub);
            }
        }
        let mut index = Self::default();
        for source in &library.sources {
            add(&mut index, library, source, &source.root);
        }
        index
    }

    pub fn get(&self, identity: &str) -> Option<&SearchTarget> {
        self.by_identity.get(identity)
    }

    /// The search bookmark of a keyword, matched without case.
    pub fn keyword(&self, keyword: &str) -> Option<&SearchTarget> {
        self.get(self.by_keyword.get(&keyword.to_lowercase())?)
    }
}

/// The search field asking for the term of a search bookmark.
#[derive(Debug)]
pub(super) struct SearchPrompt {
    pub target: SearchTarget,
    pub term: String,
    /// The query shown again when the prompt is left.
    pub back: String,
    pub placeholder: String,
}

impl SearchPrompt {
    pub fn new(target: SearchTarget, term: &str, back: String) -> Self {
        Self {
            placeholder: format!("Search {}…", target.title),
            target,
            term: term.to_string(),
            back,
        }
    }

    /// The one row of the prompt: searching for the term, or asking for it.
    pub fn items(&self, options: RowOptions) -> Vec<CommandItem> {
        let target = &self.target;
        let mut item = CommandItem::info(
            &target.identity,
            format!("Type what to search {} for", target.title),
            "",
        );
        item.icon_names = target.icon_names.clone();
        item.subtitle = target.source.clone();
        item.badge = Some(SEARCH_BADGE.to_string());
        if self.term.trim().is_empty() {
            return vec![item];
        }
        let url = expand(&target.url, &self.term);
        let action = |action| CommandAction {
            action,
            confirm: None,
            keep_open: options.keep_open,
            progress: None,
        };
        item.title = format!("Search {} for '{}'", target.title, self.term);
        item.subtitle = url.clone();
        item.details = vec![
            ("URL".to_string(), url.clone()),
            ("Source".to_string(), target.source.clone()),
        ];
        match &target.opener {
            Opener::Argv { name, .. } => {
                item.details
                    .push(("Enter".to_string(), format!("Search with {name}")));
            }
            Opener::Missing(reason) => item.unavailable = Some(reason.clone()),
        }
        item.details
            .push(("Ctrl+Enter".to_string(), "Copy the link".to_string()));
        item.primary = target.opener.action(&url).map(action);
        item.secondary = Some(action(DeferredAction::Copy(url.clone())));
        item.domains = domains_of([url.as_str()]);
        vec![item]
    }
}

fn identity(source: &Source, bookmark: &Bookmark) -> String {
    format!("{IDENTITY_PREFIX}{}:{}", source.key, bookmark.id)
}

fn title(bookmark: &Bookmark) -> String {
    if bookmark.title.trim().is_empty() {
        bookmark.url.clone()
    } else {
        bookmark.title.clone()
    }
}

/// The site icon when known, then the source's browser icon.
fn icon_names(library: &Library, source: &Source, url: &str) -> Vec<String> {
    super::favicons::host_key(url)
        .and_then(|host| library.icons.get(&host))
        .map(|path| path.to_string_lossy().into_owned())
        .into_iter()
        .chain([source.icon.clone()])
        .chain(BOOKMARK_ICONS.map(str::to_string))
        .collect()
}

/// A folder name as typed in a query.
fn segment(name: &str) -> String {
    name.replace('/', &SLASH.to_string())
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Read every configured source, or every browser profile found. Blocking:
/// run it off the UI thread.
pub(super) fn load(config: &BookmarksAddonConfig) -> Library {
    let Ok((home, config_dir)) = browser_host::user_dirs() else {
        return Library {
            errors: vec!["HOME is not set".to_string()],
            ..Library::default()
        };
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    load_from(config, &home, &config_dir, &path)
}

/// Look up the site icons of `library` in the caches and favicon databases.
/// Blocking: run it off the UI thread.
pub(super) fn lookup_icons(library: &Library) -> HashMap<String, PathBuf> {
    super::favicons::lookup_sites(
        library
            .sources
            .iter()
            .map(|source| {
                let mut hosts = BTreeSet::new();
                source.root.hosts(&mut hosts);
                (hosts, source.favicons.clone())
            })
            .collect(),
    )
}

/// One source to read.
struct Spec {
    browser: Option<BookmarkBrowser>,
    file: PathBuf,
    /// The profile directory, opened with the browser of the source.
    profile: Option<PathBuf>,
    profile_name: String,
    folder: Option<String>,
    name: Option<String>,
    open_with: Option<BookmarkOpener>,
}

fn bookmark_file(dir: &Path, gecko: bool) -> PathBuf {
    dir.join(if gecko { "places.sqlite" } else { "Bookmarks" })
}

fn profile_spec(profile: BrowserProfile) -> Spec {
    let gecko = browser_host::browser_details(profile.browser).is_some_and(|(_, _, gecko)| gecko);
    Spec {
        browser: Some(profile.browser),
        file: bookmark_file(&profile.dir, gecko),
        profile: Some(profile.dir),
        profile_name: profile.name,
        folder: None,
        name: None,
        open_with: None,
    }
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(super) fn load_from(
    config: &BookmarksAddonConfig,
    home: &Path,
    config_dir: &Path,
    path: &OsStr,
) -> Library {
    let mut library = Library::default();
    let mut specs: Vec<Spec> = Vec::new();
    match &config.sources {
        None => specs.extend(
            browser_host::bookmark_profiles(None, home, config_dir)
                .into_iter()
                .map(profile_spec),
        ),
        Some(sources) => {
            for source in sources {
                let with = |spec: Spec| Spec {
                    folder: source.folder.clone(),
                    name: source.name.clone(),
                    open_with: source.open_with.clone(),
                    ..spec
                };
                if let Some(file) = &source.path {
                    let file = PathBuf::from(file);
                    let (file, profile) = if file.is_dir() {
                        let places = bookmark_file(&file, true);
                        let file_in = if places.is_file() {
                            places
                        } else {
                            bookmark_file(&file, false)
                        };
                        (file_in, file)
                    } else {
                        let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
                        (file, dir)
                    };
                    specs.push(with(Spec {
                        browser: source.browser,
                        file,
                        profile_name: dir_name(&profile),
                        profile: Some(profile),
                        folder: None,
                        name: None,
                        open_with: None,
                    }));
                    continue;
                }
                let Some(browser) = source.browser else {
                    library
                        .errors
                        .push("A bookmarks source needs a browser or a path".to_string());
                    continue;
                };
                let browser_name =
                    browser_host::browser_details(browser).map_or("Browser", |(name, _, _)| name);
                let profiles: Vec<BrowserProfile> =
                    browser_host::bookmark_profiles(Some(browser), home, config_dir)
                        .into_iter()
                        .filter(|profile| {
                            source.profile.as_ref().is_none_or(|wanted| {
                                profile.name.eq_ignore_ascii_case(wanted)
                                    || dir_name(&profile.dir) == *wanted
                            })
                        })
                        .collect();
                if profiles.is_empty() {
                    library.errors.push(match &source.profile {
                        Some(profile) => {
                            format!("{browser_name}: no profile {profile} with bookmarks")
                        }
                        None => format!("{browser_name}: no profile with bookmarks"),
                    });
                }
                specs.extend(profiles.into_iter().map(profile_spec).map(with));
            }
        }
    }

    let mut sources: Vec<(Source, String)> = Vec::new();
    for spec in specs {
        let label = spec.name.clone().unwrap_or_else(|| {
            spec.browser
                .and_then(browser_host::browser_details)
                .map_or_else(|| dir_name(&spec.file), |(name, _, _)| name.to_string())
        });
        match read_source(&spec, config.open_with.as_ref(), path) {
            Ok(source) => sources.push((Source { label, ..source }, spec.profile_name)),
            Err(error) => library.errors.push(format!("{label}: {error}")),
        }
    }
    let duplicated = |sources: &[(Source, String)], label: &str| {
        sources
            .iter()
            .filter(|(source, _)| source.label == label)
            .count()
            > 1
    };
    let labels: Vec<String> = sources
        .iter()
        .map(|(source, profile)| {
            if duplicated(&sources, &source.label) && !profile.is_empty() {
                format!("{} {profile}", source.label)
            } else {
                source.label.clone()
            }
        })
        .collect();
    for ((source, _), label) in sources.iter_mut().zip(labels) {
        source.label = label;
    }
    let mut keys: Vec<String> = Vec::new();
    for (mut source, _) in sources {
        let mut key = source.key.clone();
        let mut n = 1;
        while keys.contains(&key) {
            n += 1;
            key = format!("{}#{n}", source.key);
        }
        keys.push(key.clone());
        source.key = key;
        library.sources.push(source);
    }
    library
}

fn read_source(
    spec: &Spec,
    default_opener: Option<&BookmarkOpener>,
    path: &OsStr,
) -> Result<Source, String> {
    let mut magic = [0u8; 16];
    fs::File::open(&spec.file)
        .and_then(|mut file| file.read_exact(&mut magic))
        .map_err(|error| format!("cannot read {}: {error}", spec.file.display()))?;
    let gecko = &magic == b"SQLite format 3\0";
    let mut root = if gecko {
        read_places(&spec.file)?
    } else {
        let text = fs::read_to_string(&spec.file)
            .map_err(|error| format!("cannot read {}: {error}", spec.file.display()))?;
        parse_chromium(&text)?
    };
    if let Some(folder) = spec
        .folder
        .as_deref()
        .filter(|folder| !folder.trim().is_empty())
    {
        let segments: Vec<&str> = folder.split('/').filter(|part| !part.is_empty()).collect();
        root = root
            .walk(&segments)
            .map(|(found, _)| found.clone())
            .ok_or_else(|| format!("no folder {folder}"))?;
    }
    let details = spec.browser.and_then(browser_host::browser_details);
    let icon = details
        .map_or(BOOKMARK_ICONS[0], |(_, icon, _)| icon)
        .to_string();
    let favicons = spec
        .profile
        .iter()
        .map(|dir| {
            if gecko {
                (dir.join("favicons.sqlite"), true)
            } else {
                (dir.join("Favicons"), false)
            }
        })
        .filter(|(file, _)| file.is_file())
        .collect();
    let mut key = format!(
        "{}:{}",
        spec.browser.map_or_else(
            || "file".to_string(),
            |browser| format!("{browser:?}").to_lowercase()
        ),
        spec.profile.as_deref().map(dir_name).unwrap_or_default()
    );
    if let Some(folder) = &spec.folder {
        key = format!("{key}:{folder}");
    }
    Ok(Source {
        key,
        label: String::new(),
        icon,
        root,
        opener: opener(
            spec.open_with.as_ref().or(default_opener),
            spec.browser,
            spec.profile.as_deref(),
            path,
        ),
        favicons,
    })
}

/// What opens the links of a source of `browser` read from `profile`.
fn opener(
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
            if browser_host::find_program(&[program.as_str()], path).is_none() {
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

/// Open in `target`, in the profile the bookmarks come from when it is
/// their browser.
fn browser_opener(
    target: BookmarkBrowser,
    source: Option<BookmarkBrowser>,
    profile: Option<&Path>,
    path: &OsStr,
) -> Opener {
    let Some((name, _, gecko)) = browser_host::browser_details(target) else {
        return Opener::Missing("unknown browser".to_string());
    };
    let Some(program) = browser_host::browser_program(target, path) else {
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
        apps: browser_host::browser_window_apps(target),
    }
}

/// The roots of a Chromium `Bookmarks` file.
fn parse_chromium(text: &str) -> Result<Folder, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("not a bookmarks file: {error}"))?;
    let roots = value
        .get("roots")
        .and_then(Value::as_object)
        .ok_or("not a bookmarks file: no roots")?;
    let mut root = Folder::default();
    for (key, default) in CHROMIUM_ROOTS {
        let Some(node) = roots.get(key) else {
            continue;
        };
        let mut folder = chromium_folder(node, 0);
        if folder.name == UNTITLED {
            folder.name = default.to_string();
        }
        if !folder.is_empty() {
            root.folders.push(folder);
        }
    }
    Ok(root)
}

fn chromium_folder(node: &Value, depth: usize) -> Folder {
    let text = |node: &Value, key: &str| {
        node.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut folder = Folder {
        name: Some(text(node, "name"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| UNTITLED.to_string()),
        ..Folder::default()
    };
    if depth >= MAX_DEPTH {
        return folder;
    }
    for child in node
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match child.get("type").and_then(Value::as_str) {
            Some("folder") => folder.folders.push(chromium_folder(child, depth + 1)),
            Some("url") => {
                let id = Some(text(child, "guid"))
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| text(child, "id"));
                let added = text(child, "date_added")
                    .parse::<i64>()
                    .ok()
                    .filter(|&micros| micros > 0)
                    .map(|micros| (micros - CHROMIUM_EPOCH_OFFSET) / 1_000_000);
                folder.bookmarks.push(Bookmark {
                    title: text(child, "name"),
                    url: text(child, "url"),
                    id,
                    added,
                    keyword: None,
                });
            }
            _ => {}
        }
    }
    folder
}

/// A private copy of a Firefox database and its write-ahead log, which
/// holds the latest changes while the browser runs; removed when dropped.
struct PlacesCopy {
    dir: PathBuf,
}

impl Drop for PlacesCopy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn copy_places(file: &Path) -> Result<(PlacesCopy, PathBuf), String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "raffi-bookmarks-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let copy = PlacesCopy { dir };
    let target = copy.dir.join("places.sqlite");
    fs::copy(file, &target).map_err(|error| format!("cannot copy {}: {error}", file.display()))?;
    let mut wal = file.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).is_file() {
        let _ = fs::copy(&wal, copy.dir.join("places.sqlite-wal"));
    }
    Ok((copy, target))
}

fn read_places(file: &Path) -> Result<Folder, String> {
    let (_copy, target) = copy_places(file)?;
    let connection = Connection::open_with_flags(
        &target,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("cannot open {}: {error}", file.display()))?;
    places_tree(&connection).map_err(|error| format!("cannot read {}: {error}", file.display()))
}

struct PlacesRow {
    id: i64,
    kind: i64,
    title: String,
    guid: String,
    added: Option<i64>,
    url: Option<String>,
    keyword: Option<String>,
}

fn places_tree(connection: &Connection) -> rusqlite::Result<Folder> {
    let has_keywords = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'moz_keywords'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    // Keywords posting a form cannot be opened from a command line.
    let keyword = if has_keywords {
        "(SELECT k.keyword FROM moz_keywords k WHERE k.place_id = b.fk \
         AND COALESCE(k.post_data, '') = '' ORDER BY k.id LIMIT 1)"
    } else {
        "NULL"
    };
    let mut statement = connection.prepare(&format!(
        "SELECT b.id, b.type, b.parent, COALESCE(b.title, ''), b.guid, b.dateAdded, p.url, \
         {keyword} FROM moz_bookmarks b LEFT JOIN moz_places p ON p.id = b.fk \
         ORDER BY b.parent, b.position",
    ))?;
    let mut children: HashMap<i64, Vec<PlacesRow>> = HashMap::new();
    let mut roots: HashMap<String, i64> = HashMap::new();
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(2)?,
            PlacesRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(3)?,
                guid: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                added: row.get(5)?,
                url: row.get(6)?,
                keyword: row.get(7)?,
            },
        ))
    })?;
    for row in rows {
        let (parent, row) = row?;
        roots.insert(row.guid.clone(), row.id);
        children.entry(parent).or_default().push(row);
    }
    let mut root = Folder::default();
    for (guid, name) in GECKO_ROOTS {
        let Some(&id) = roots.get(guid) else {
            continue;
        };
        let folder = places_folder(&children, id, name.to_string(), 0);
        if !folder.is_empty() {
            root.folders.push(folder);
        }
    }
    Ok(root)
}

fn places_folder(
    children: &HashMap<i64, Vec<PlacesRow>>,
    id: i64,
    name: String,
    depth: usize,
) -> Folder {
    let mut folder = Folder {
        name,
        ..Folder::default()
    };
    if depth >= MAX_DEPTH {
        return folder;
    }
    for row in children.get(&id).into_iter().flatten() {
        match (row.kind, &row.url) {
            (1, Some(url)) if !url.starts_with("place:") => folder.bookmarks.push(Bookmark {
                title: row.title.clone(),
                url: url.clone(),
                id: row.guid.clone(),
                added: row.added.map(|micros| micros / 1_000_000),
                keyword: row.keyword.clone(),
            }),
            (2, _) => {
                let name = Some(row.title.clone())
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| UNTITLED.to_string());
                folder
                    .folders
                    .push(places_folder(children, row.id, name, depth + 1));
            }
            _ => {}
        }
    }
    folder
}

/// The rows for the folder path of a query and the query path each folder
/// row opens.
#[derive(Debug, Default)]
pub(super) struct Rows {
    pub items: Vec<CommandItem>,
    pub folders: HashMap<String, String>,
}

/// What rows show and how actions behave.
#[derive(Debug, Clone, Copy)]
pub(super) struct RowOptions {
    pub keep_open: bool,
}

struct Builder<'a> {
    library: &'a Library,
    options: RowOptions,
    rows: Rows,
}

impl Builder<'_> {
    fn folder(&mut self, source: &Source, folder: &Folder, rel: &str, path: String) {
        let count = folder.count();
        let count = if count == 1 {
            "1 bookmark".to_string()
        } else {
            format!("{count} bookmarks")
        };
        let identity = format!("bookmark-folder:{}:{path}", source.key);
        let mut item = CommandItem::info(&identity, folder.name.clone(), FOLDER_ICON);
        item.subtitle = if rel.is_empty() {
            count.clone()
        } else {
            format!("{rel} · {count}")
        };
        item.match_text = join(rel, &folder.name);
        item.sort_priority = 1;
        item.details = vec![
            ("Folder".to_string(), path.trim_end_matches('/').to_string()),
            ("Bookmarks".to_string(), count),
            ("Enter".to_string(), "Open the folder".to_string()),
        ];
        self.rows.folders.insert(identity, path);
        self.rows.items.push(item);
    }

    fn source(&mut self, source: &Source) {
        let identity = format!("bookmark-source:{}", source.key);
        let mut item = CommandItem::info(&identity, source.label.clone(), &source.icon);
        item.icon_names.push(FOLDER_ICON.to_string());
        let count = source.root.count();
        item.subtitle = if count == 1 {
            "1 bookmark".to_string()
        } else {
            format!("{count} bookmarks")
        };
        item.match_text = source.label.clone();
        item.sort_priority = 1;
        item.details = vec![("Enter".to_string(), "Open the bookmarks".to_string())];
        self.rows
            .folders
            .insert(identity, format!("{}/", segment(&source.label)));
        self.rows.items.push(item);
    }

    fn bookmark(&mut self, source: &Source, bookmark: &Bookmark, rel: &str) {
        let title = title(bookmark);
        let search = is_search(&bookmark.url);
        let action = |action| CommandAction {
            action,
            confirm: None,
            keep_open: self.options.keep_open,
            progress: None,
        };
        let mut details = vec![("URL".to_string(), bookmark.url.clone())];
        if !rel.is_empty() {
            details.push(("Folder".to_string(), rel.to_string()));
        }
        details.push(("Source".to_string(), source.label.clone()));
        if let Some(added) = bookmark
            .added
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        {
            details.push(("Added".to_string(), added.format("%Y-%m-%d").to_string()));
        }
        let keyword = bookmark.keyword.as_deref().unwrap_or_default();
        if !keyword.is_empty() {
            details.push(("Keyword".to_string(), keyword.to_string()));
        }
        let unavailable = match &source.opener {
            Opener::Argv { name, .. } => {
                let enter = if search {
                    format!("Search with {name}")
                } else {
                    format!("Open in {name}")
                };
                details.push(("Enter".to_string(), enter));
                None
            }
            Opener::Missing(reason) => Some(reason.clone()),
        };
        details.push(("Ctrl+Enter".to_string(), "Copy the link".to_string()));
        let host = uri_host(&bookmark.url).unwrap_or_default();
        self.rows.items.push(CommandItem {
            subtitle: match (rel.is_empty(), host.is_empty()) {
                (true, _) => bookmark.url.clone(),
                (false, true) => rel.to_string(),
                (false, false) => format!("{rel} · {host}"),
            },
            match_text: format!("{title} {} {rel} {keyword}", bookmark.url),
            icon_names: icon_names(self.library, source, &bookmark.url),
            icon: None,
            identity: identity(source, bookmark),
            unavailable,
            primary: source.opener.action(&bookmark.url).map(action),
            secondary: Some(action(DeferredAction::Copy(bookmark.url.clone()))),
            details,
            badge: search.then(|| SEARCH_BADGE.to_string()),
            domains: domains_of([bookmark.url.as_str()]),
            dimmed: false,
            typed_only: false,
            sort_priority: 0,
            title,
        });
    }

    /// The folders and bookmarks of `folder`; with `deep`, everything below.
    fn collect(
        &mut self,
        source: &Source,
        folder: &Folder,
        rel: &str,
        base: &str,
        deep: bool,
        folders: bool,
    ) {
        for sub in &folder.folders {
            let sub_rel = join(rel, &segment(&sub.name));
            if folders {
                self.folder(source, sub, rel, format!("{base}{sub_rel}/"));
            }
            if deep {
                self.collect(source, sub, &sub_rel, base, deep, folders);
            }
        }
        for bookmark in &folder.bookmarks {
            self.bookmark(source, bookmark, rel);
        }
    }
}

/// The rows in folder `path` of the sources of `view`: the direct folders
/// and bookmarks, or with `filtered` every one below for the filter to pick
/// from. Several sources start with a folder per source.
pub(super) fn rows(
    library: &Library,
    view: Option<usize>,
    path: &str,
    filtered: bool,
    options: RowOptions,
) -> Rows {
    let mut builder = Builder {
        library,
        options,
        rows: Rows::default(),
    };
    let sources: Vec<&Source> = match view.and_then(|view| library.sources.get(view)) {
        Some(source) => vec![source],
        None => library.sources.iter().collect(),
    };
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if segments.is_empty() {
        for (index, error) in library.errors.iter().enumerate() {
            let mut item = CommandItem::info(
                &format!("bookmark-error:{index}"),
                error.clone(),
                "dialog-warning",
            );
            item.subtitle = "Bookmarks source skipped".to_string();
            builder.rows.items.push(item);
        }
        if sources.is_empty() && library.errors.is_empty() {
            builder.rows.items.push(CommandItem::info(
                "bookmark-none",
                "No browser bookmarks found".to_string(),
                "dialog-information",
            ));
        }
    }
    let (source, segments, base) = match sources.as_slice() {
        [source] => (*source, &segments[..], String::new()),
        _ => match segments.split_first() {
            None => {
                for source in &sources {
                    if filtered {
                        let label = segment(&source.label);
                        builder.collect(source, &source.root, &label, "", true, true);
                    } else {
                        builder.source(source);
                    }
                }
                return builder.rows;
            }
            Some((first, rest)) => {
                let Some(source) = sources
                    .iter()
                    .find(|source| segment(&source.label).to_lowercase() == first.to_lowercase())
                else {
                    return missing(builder.rows, path);
                };
                (*source, rest, format!("{}/", segment(&source.label)))
            }
        },
    };
    let Some((folder, names)) = source.root.walk(segments) else {
        return missing(builder.rows, path);
    };
    let base = names
        .iter()
        .fold(base, |base, name| format!("{base}{name}/"));
    builder.collect(source, folder, "", &base, filtered, true);
    builder.rows
}

fn missing(mut rows: Rows, path: &str) -> Rows {
    rows.items.push(CommandItem::info(
        "bookmark-missing",
        format!("No folder {}", path.trim_end_matches('/')),
        "dialog-information",
    ));
    rows
}

/// Every bookmark of every source, for the main view.
pub(super) fn flat_items(library: &Library, options: RowOptions) -> Vec<CommandItem> {
    let mut builder = Builder {
        library,
        options,
        rows: Rows::default(),
    };
    let several = library.sources.len() > 1;
    for source in &library.sources {
        let rel = if several {
            segment(&source.label)
        } else {
            String::new()
        };
        builder.collect(source, &source.root, &rel, "", true, false);
    }
    builder.rows.items
}

#[cfg(test)]
pub(super) fn fixture_library() -> Library {
    let bookmark = |title: &str, url: &str, id: &str| Bookmark {
        title: title.to_string(),
        url: url.to_string(),
        id: id.to_string(),
        added: Some(1_700_000_000),
        keyword: None,
    };
    let opener = Opener::Argv {
        argv: vec!["firefox".to_string(), "{url}".to_string()],
        name: "Firefox".to_string(),
        apps: Vec::new(),
    };
    Library {
        sources: vec![
            Source {
                key: "firefox:abc.default".to_string(),
                label: "Firefox".to_string(),
                icon: "firefox".to_string(),
                root: Folder {
                    name: String::new(),
                    folders: vec![
                        Folder {
                            name: "Bookmarks Toolbar".to_string(),
                            folders: vec![Folder {
                                name: "Work".to_string(),
                                folders: Vec::new(),
                                bookmarks: vec![
                                    bookmark("Raffi", "https://github.com/chmouel/raffi", "w1"),
                                    bookmark("Rust docs", "https://doc.rust-lang.org/std/", "w2"),
                                ],
                            }],
                            bookmarks: vec![bookmark("Mail", "https://mail.example.com/", "t1")],
                        },
                        Folder {
                            name: "Other Bookmarks".to_string(),
                            folders: Vec::new(),
                            bookmarks: vec![
                                bookmark("Recipes", "https://recipes.example.org/", "o1"),
                                Bookmark {
                                    keyword: Some("nixsearch".to_string()),
                                    ..bookmark(
                                        "Nix code search",
                                        "https://github.com/search?q=%22%s%22+path%3A*.nix",
                                        "o2",
                                    )
                                },
                            ],
                        },
                    ],
                    bookmarks: Vec::new(),
                },
                opener: opener.clone(),
                favicons: Vec::new(),
            },
            Source {
                key: "brave:Default".to_string(),
                label: "Brave".to_string(),
                icon: "brave-browser".to_string(),
                root: Folder {
                    name: String::new(),
                    folders: vec![Folder {
                        name: "Bookmarks bar".to_string(),
                        folders: Vec::new(),
                        bookmarks: vec![bookmark("News", "https://news.example.net/", "b1")],
                    }],
                    bookmarks: Vec::new(),
                },
                opener,
                favicons: Vec::new(),
            },
        ],
        errors: Vec::new(),
        icons: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BookmarkSourceConfig;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    const OPTIONS: RowOptions = RowOptions { keep_open: false };

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("raffi-bookmarks-test-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn program(dir: &Path, name: &str) {
        let file = dir.join(name);
        fs::write(&file, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }

    const CHROMIUM: &str = r#"{"roots": {
        "bookmark_bar": {"type": "folder", "name": "Bookmarks bar", "children": [
            {"type": "url", "name": "Raffi", "url": "https://github.com/chmouel/raffi",
             "guid": "g1", "date_added": "13350000000000000"},
            {"type": "folder", "name": "Work/Home", "children": [
                {"type": "url", "name": "", "url": "https://example.com/a", "id": "7"}
            ]}
        ]},
        "other": {"type": "folder", "name": "Other bookmarks", "children": []},
        "synced": {"type": "folder", "name": "", "children": [
            {"type": "url", "name": "Phone", "url": "https://m.example.com/", "guid": "g3"}
        ]}
    }}"#;

    fn places(file: &Path) {
        let connection = Connection::open(file).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE moz_places (id INTEGER PRIMARY KEY, url TEXT);
                 CREATE TABLE moz_bookmarks (id INTEGER PRIMARY KEY, type INTEGER, fk INTEGER,
                    parent INTEGER, position INTEGER, title TEXT, guid TEXT, dateAdded INTEGER);
                 INSERT INTO moz_places VALUES (1, 'https://github.com/chmouel/raffi'),
                    (2, 'place:sort=8'), (3, 'https://doc.rust-lang.org/');
                 INSERT INTO moz_bookmarks VALUES
                    (1, 2, NULL, 0, 0, '', 'root________', 0),
                    (2, 2, NULL, 1, 0, 'menu', 'menu________', 0),
                    (3, 2, NULL, 1, 1, 'toolbar', 'toolbar_____', 0),
                    (4, 2, NULL, 1, 2, 'tags', 'tags________', 0),
                    (5, 2, NULL, 1, 3, 'unfiled', 'unfiled_____', 0),
                    (10, 1, 1, 3, 1, 'Raffi', 'aaaaaaaaaaaa', 1700000000000000),
                    (11, 1, 2, 3, 0, 'Most Visited', 'bbbbbbbbbbbb', 0),
                    (12, 2, NULL, 3, 2, 'Work', 'cccccccccccc', 0),
                    (13, 1, 3, 12, 0, 'Rust', 'dddddddddddd', 0),
                    (14, 3, NULL, 12, 1, NULL, 'eeeeeeeeeeee', 0),
                    (15, 1, 3, 4, 0, 'rust', 'ffffffffffff', 0);",
            )
            .unwrap();
    }

    #[test]
    fn test_parse_chromium_names_roots_and_skips_empty_ones() {
        let root = parse_chromium(CHROMIUM).unwrap();
        let names: Vec<&str> = root.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["Bookmarks bar", "Mobile bookmarks"]);
        let bar = &root.folders[0];
        assert_eq!(bar.bookmarks[0].id, "g1");
        assert_eq!(bar.bookmarks[0].added, Some(1_705_526_400));
        assert_eq!(bar.folders[0].name, "Work/Home");
        assert_eq!(bar.folders[0].bookmarks[0].id, "7");
        assert_eq!(root.count(), 3);
        assert!(parse_chromium("{}").is_err());
    }

    #[test]
    fn test_read_places_uses_firefox_roots_and_skips_tags_queries_and_separators() {
        let dir = temp_dir("places");
        places(&dir.join("places.sqlite"));
        let root = read_places(&dir.join("places.sqlite")).unwrap();
        let names: Vec<&str> = root.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["Bookmarks Toolbar"]);
        let toolbar = &root.folders[0];
        assert_eq!(toolbar.bookmarks.len(), 1);
        assert_eq!(toolbar.bookmarks[0].title, "Raffi");
        assert_eq!(toolbar.bookmarks[0].added, Some(1_700_000_000));
        assert_eq!(toolbar.folders[0].name, "Work");
        assert_eq!(
            toolbar.folders[0].bookmarks[0].url,
            "https://doc.rust-lang.org/"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_read_places_takes_keywords_without_post_data() {
        let dir = temp_dir("keywords");
        let file = dir.join("places.sqlite");
        places(&file);
        Connection::open(&file)
            .unwrap()
            .execute_batch(
                "CREATE TABLE moz_keywords (id INTEGER PRIMARY KEY, keyword TEXT,
                    place_id INTEGER, post_data TEXT);
                 INSERT INTO moz_places VALUES (4, 'https://search.example/?q=%s');
                 INSERT INTO moz_bookmarks VALUES
                    (16, 1, 4, 3, 3, 'Search', 'gggggggggggg', 0);
                 INSERT INTO moz_keywords VALUES (1, 'rs', 3, 'q=%s'), (2, 'se', 4, NULL);",
            )
            .unwrap();
        let root = read_places(&file).unwrap();
        let toolbar = &root.folders[0];
        assert_eq!(toolbar.bookmarks[1].keyword.as_deref(), Some("se"));
        assert_eq!(toolbar.bookmarks[0].keyword, None);
        assert_eq!(toolbar.folders[0].bookmarks[0].keyword, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_search_urls_expand_their_term() {
        let url = "https://x.example/?q=%s&raw=%S&again=%s&keep=%20";
        assert!(is_search(url));
        assert!(!is_search("https://x.example/?q=%20"));
        assert_eq!(
            expand(url, "a b/c"),
            "https://x.example/?q=a%20b%2Fc&raw=a b/c&again=a%20b%2Fc&keep=%20"
        );
        assert_eq!(expand("https://x.example/", "a"), "https://x.example/");
        assert_eq!(keyword_query("nix  flake"), Some(("nix", "flake")));
        assert_eq!(keyword_query("nix "), Some(("nix", "")));
        assert_eq!(keyword_query("nix"), None);
        assert_eq!(keyword_query(" nix"), None);
    }

    #[test]
    fn test_search_index_finds_keywords_without_case_and_prefers_the_first_source() {
        let mut library = fixture_library();
        library.sources[1].root.folders[0].bookmarks.push(Bookmark {
            title: "Other".to_string(),
            url: "https://other.example/?q=%s".to_string(),
            id: "b2".to_string(),
            added: None,
            keyword: Some("NixSearch".to_string()),
        });
        let index = SearchIndex::new(&library);
        let target = index.keyword("NIXSEARCH").unwrap();
        assert_eq!(target.identity, "bookmark:firefox:abc.default:o2");
        assert!(index.get("bookmark:brave:Default:b2").is_some());
        assert!(index.get("bookmark:firefox:abc.default:o1").is_none());
        assert!(index.keyword("recipes").is_none());
    }

    #[test]
    fn test_search_prompt_asks_for_a_term_then_opens_or_copies_the_expanded_url() {
        let index = SearchIndex::new(&fixture_library());
        let target = index.keyword("nixsearch").unwrap().clone();
        let mut prompt = SearchPrompt::new(target, " ", "bm ".to_string());
        assert_eq!(prompt.placeholder, "Search Nix code search…");
        let items = prompt.items(OPTIONS);
        assert_eq!(items[0].title, "Type what to search Nix code search for");
        assert!(items[0].primary.is_none());

        prompt.term = "mkShell".to_string();
        let items = prompt.items(OPTIONS);
        let url = "https://github.com/search?q=%22mkShell%22+path%3A*.nix";
        assert_eq!(items[0].title, "Search Nix code search for 'mkShell'");
        assert_eq!(items[0].identity, "bookmark:firefox:abc.default:o2");
        assert!(matches!(
            &items[0].primary.as_ref().unwrap().action,
            DeferredAction::Command(argv) if argv == &["firefox", url]
        ));
        assert!(matches!(
            &items[0].secondary.as_ref().unwrap().action,
            DeferredAction::Copy(copied) if copied == url
        ));
    }

    #[test]
    fn test_load_finds_profiles_names_sources_and_opens_in_their_profile() {
        let home = temp_dir("load");
        let config = home.join(".config");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        program(&bin, "firefox");
        program(&bin, "brave");
        let firefox = home.join(".mozilla/firefox");
        fs::create_dir_all(firefox.join("abc.default-release")).unwrap();
        fs::write(
            firefox.join("profiles.ini"),
            "[Profile0]\nName=default-release\nIsRelative=1\nPath=abc.default-release\n",
        )
        .unwrap();
        places(&firefox.join("abc.default-release/places.sqlite"));
        let brave = config.join("BraveSoftware/Brave-Browser");
        for profile in ["Default", "Profile 1"] {
            fs::create_dir_all(brave.join(profile)).unwrap();
            fs::write(brave.join(profile).join("Bookmarks"), CHROMIUM).unwrap();
        }
        fs::write(
            brave.join("Local State"),
            r#"{"profile": {"info_cache": {"Default": {"name": "Personal"}, "Profile 1": {"name": "Work"}}}}"#,
        )
        .unwrap();
        // Opera's profile is its root directory.
        program(&bin, "opera");
        fs::create_dir_all(config.join("opera")).unwrap();
        fs::write(config.join("opera/Bookmarks"), CHROMIUM).unwrap();

        let library = load_from(
            &BookmarksAddonConfig::default(),
            &home,
            &config,
            bin.as_os_str(),
        );
        assert!(library.errors.is_empty(), "{:?}", library.errors);
        let labels: Vec<&str> = library.sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Firefox", "Brave Personal", "Brave Work", "Opera"]
        );
        let Opener::Argv { argv, .. } = &library.sources[0].opener else {
            panic!("no opener");
        };
        assert_eq!(
            library.sources[0]
                .opener
                .command("https://x.example/")
                .unwrap(),
            vec![
                bin.join("firefox").to_string_lossy().into_owned(),
                "--profile".to_string(),
                firefox
                    .join("abc.default-release")
                    .to_string_lossy()
                    .into_owned(),
                "https://x.example/".to_string(),
            ]
        );
        assert_eq!(argv.len(), 4);
        assert!(matches!(
            library.sources[1].opener.action("u"),
            Some(DeferredAction::OpenInApp { apps, .. }) if apps == ["brave-browser", "brave"]
        ));
        assert_eq!(
            library.sources[2].opener.command("u").unwrap()[1],
            "--profile-directory=Profile 1"
        );
        assert_eq!(library.sources[3].opener.command("u").unwrap().len(), 2);

        // Explicit sources replace auto-detection.
        let explicit = BookmarksAddonConfig {
            open_with: Some(BookmarkOpener::Browser(BookmarkBrowser::Firefox)),
            sources: Some(vec![
                BookmarkSourceConfig {
                    browser: Some(BookmarkBrowser::Brave),
                    profile: Some("work".to_string()),
                    folder: Some("bookmarks BAR/work∕home".to_string()),
                    ..BookmarkSourceConfig::default()
                },
                BookmarkSourceConfig {
                    path: Some(brave.join("Default").to_string_lossy().into_owned()),
                    name: Some("Synced".to_string()),
                    open_with: Some(BookmarkOpener::Command(vec!["qutebrowser".to_string()])),
                    ..BookmarkSourceConfig::default()
                },
                BookmarkSourceConfig {
                    browser: Some(BookmarkBrowser::Chrome),
                    ..BookmarkSourceConfig::default()
                },
                BookmarkSourceConfig {
                    browser: Some(BookmarkBrowser::Firefox),
                    folder: Some("Nope".to_string()),
                    ..BookmarkSourceConfig::default()
                },
            ]),
            ..BookmarksAddonConfig::default()
        };
        let library = load_from(&explicit, &home, &config, bin.as_os_str());
        assert_eq!(
            library.errors,
            vec![
                "Google Chrome: no profile with bookmarks".to_string(),
                "Firefox: no folder Nope".to_string(),
            ]
        );
        assert_eq!(library.sources.len(), 2);
        let work = &library.sources[0];
        assert_eq!(work.label, "Brave");
        assert_eq!(work.root.name, "Work/Home");
        // Firefox opens the Brave bookmarks, without a Brave profile flag.
        assert_eq!(
            work.opener.command("u").unwrap(),
            vec![
                bin.join("firefox").to_string_lossy().into_owned(),
                "u".to_string()
            ]
        );
        assert_eq!(
            library.sources[1].opener,
            Opener::Missing("qutebrowser is not installed".to_string())
        );
        program(&bin, "qutebrowser");
        let library = load_from(&explicit, &home, &config, bin.as_os_str());
        assert_eq!(
            library.sources[1].opener.command("u").unwrap(),
            vec!["qutebrowser".to_string(), "u".to_string()]
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn test_opener_falls_back_to_the_default_browser_only_by_default() {
        let empty = OsStr::new("");
        assert_eq!(
            opener(None, Some(BookmarkBrowser::Firefox), None, empty)
                .command("u")
                .unwrap(),
            vec!["xdg-open", "u"]
        );
        assert_eq!(
            opener(
                Some(&BookmarkOpener::Browser(BookmarkBrowser::Chrome)),
                Some(BookmarkBrowser::Firefox),
                None,
                empty
            ),
            Opener::Missing("Google Chrome is not installed".to_string())
        );
    }

    fn titles(rows: &Rows) -> Vec<&str> {
        rows.items.iter().map(|item| item.title.as_str()).collect()
    }

    #[test]
    fn test_rows_list_sources_then_folders_then_everything_below_when_filtered() {
        let library = fixture_library();
        let top = rows(&library, None, "", false, OPTIONS);
        assert_eq!(titles(&top), vec!["Firefox", "Brave"]);
        assert_eq!(top.folders["bookmark-source:brave:Default"], "Brave/");

        let firefox = rows(&library, None, "firefox/", false, OPTIONS);
        assert_eq!(
            titles(&firefox),
            vec!["Bookmarks Toolbar", "Other Bookmarks"]
        );
        assert_eq!(
            firefox.folders.values().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                &"Firefox/Bookmarks Toolbar/".to_string(),
                &"Firefox/Other Bookmarks/".to_string()
            ])
        );

        let toolbar = rows(&library, None, "Firefox/bookmarks toolbar/", false, OPTIONS);
        assert_eq!(titles(&toolbar), vec!["Work", "Mail"]);
        assert_eq!(toolbar.items[0].sort_priority, 1);
        assert_eq!(toolbar.items[1].subtitle, "https://mail.example.com/");

        let everything = rows(&library, None, "", true, OPTIONS);
        assert_eq!(everything.items.len(), 10);
        let raffi = everything
            .items
            .iter()
            .find(|item| item.title == "Raffi")
            .unwrap();
        assert_eq!(
            raffi.subtitle,
            "Firefox/Bookmarks Toolbar/Work · github.com"
        );
        assert_eq!(raffi.identity, "bookmark:firefox:abc.default:w1");
        assert!(matches!(
            &raffi.secondary.as_ref().unwrap().action,
            DeferredAction::Copy(url) if url == "https://github.com/chmouel/raffi"
        ));
        assert!(matches!(
            &raffi.primary.as_ref().unwrap().action,
            DeferredAction::Command(argv) if argv == &["firefox", "https://github.com/chmouel/raffi"]
        ));

        let missing = rows(&library, None, "Firefox/Nope/", false, OPTIONS);
        assert_eq!(titles(&missing), vec!["No folder Firefox/Nope"]);
    }

    #[test]
    fn test_rows_of_one_source_start_at_its_roots() {
        let library = fixture_library();
        let brave = rows(&library, Some(1), "", false, OPTIONS);
        assert_eq!(titles(&brave), vec!["Bookmarks bar"]);
        assert_eq!(brave.folders.values().next().unwrap(), "Bookmarks bar/");
        let bar = rows(&library, Some(1), "Bookmarks bar/", false, OPTIONS);
        assert_eq!(titles(&bar), vec!["News"]);
    }

    #[test]
    fn test_flat_items_hold_bookmarks_only_with_the_identities_of_the_rows() {
        let library = fixture_library();
        let items = flat_items(&library, OPTIONS);
        assert_eq!(items.len(), 6);
        assert!(items.iter().all(|item| item.primary.is_some()));
        assert_eq!(items[0].identity, "bookmark:firefox:abc.default:w1");
    }

    #[test]
    fn test_split_query_and_views() {
        assert_eq!(split_query("rust"), ("", "rust"));
        assert_eq!(split_query("Firefox/Work/ru"), ("Firefox/Work/", "ru"));
        assert_eq!(split_query("Firefox/"), ("Firefox/", ""));

        let mut state = BookmarksState {
            library: Some(Arc::new(fixture_library())),
            ..BookmarksState::default()
        };
        assert_eq!(state.view_label().as_deref(), Some("All bookmarks"));
        assert_eq!(state.next_view(false), Some(0));
        assert_eq!(state.next_view(true), Some(1));
        state.view = Some(1);
        assert_eq!(state.view_label().as_deref(), Some("Brave"));
        assert_eq!(state.next_view(false), None);
        assert_eq!(state.next_view(true), Some(0));
    }
}
