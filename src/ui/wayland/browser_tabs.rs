use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDateTime};

use crate::browser_host::{self, HostSetup, InstalledBrowser, Listing, Request, Tab};

use super::actions::DeferredAction;
use super::command_list::{domains_of, CommandAction, CommandItem, RowGroup};
use super::url_match::uri_host;

pub(super) const NO_BROWSER: &str =
    "No browser connected.\nOpen a browser with the Vicinae extension, or reload the extension.";
pub(super) const NO_TABS: &str = "No open tabs";
pub(super) const SETUP_GUIDE_URL: &str = "https://chmouel.github.io/raffi/features/browser-tabs/";
const FALLBACK_ICON: &str = "web-browser";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TabOperation {
    Focus,
    Close,
}

/// A tab switch or close sent to the browser host listening on `socket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TabAction {
    pub socket: PathBuf,
    pub tab_id: i64,
    pub operation: TabOperation,
    /// The browser process, whose window is raised after a switch.
    pub browser_pid: Option<u32>,
    /// The browser, such as `firefox`, whose profiles hold its history.
    pub browser_id: String,
    pub title: String,
    pub url: String,
}

/// The tab `item` switches to, for rows that are tabs.
pub(super) fn tab_target(item: &CommandItem) -> Option<&TabAction> {
    match &item.primary.as_ref()?.action {
        DeferredAction::BrowserTab(action) => Some(action),
        _ => None,
    }
}

/// The tabs of every connected browser, or the steps to connect one.
/// Blocking: run it off the UI thread.
pub(super) fn load_tabs(
    view: View,
    focus_keep_open: bool,
    close_keep_open: bool,
) -> Result<Vec<CommandItem>, String> {
    let dir = browser_host::socket_dir()
        .map_err(|error| format!("Cannot use the browser socket directory: {error}"))?;
    let browsers = browser_host::list_browsers(&dir);
    if browsers.is_empty() {
        let (setup, installed) = browser_host::user_dirs()
            .map(|(home, config)| {
                let path = std::env::var_os("PATH").unwrap_or_default();
                (
                    browser_host::host_setup(&home, &config),
                    browser_host::installed_browsers(&home, &config, &path),
                )
            })
            .unwrap_or((HostSetup::None, Vec::new()));
        return Ok(setup_items(&setup, &installed));
    }
    let icons = super::favicons::lookup(&browsers);
    tab_items_with(&browsers, &icons, view, focus_keep_open, close_keep_open)
}

/// Rows that connect the browsers to Raffi: write the manifests, get the
/// extension for each browser lacking it, open the setup guide.
pub(super) fn setup_items(setup: &HostSetup, browsers: &[InstalledBrowser]) -> Vec<CommandItem> {
    let mut items: Vec<CommandItem> = connect_item(setup).into_iter().collect();
    items.extend(browsers.iter().map(browser_item));
    if items.is_empty() {
        items.push(CommandItem::info(
            "setup:none",
            NO_BROWSER.to_string(),
            "dialog-information",
        ));
    }
    let mut guide = setup_row(
        "setup:guide",
        "Open the browser tabs setup guide".to_string(),
        "Opens the Raffi documentation in your default browser".to_string(),
        "help-browser",
        Some(CommandAction {
            action: DeferredAction::Command(vec![
                "xdg-open".to_string(),
                SETUP_GUIDE_URL.to_string(),
            ]),
            confirm: None,
            keep_open: false,
            progress: None,
        }),
    );
    guide.details = vec![("URL".to_string(), SETUP_GUIDE_URL.to_string())];
    items.push(guide);
    items
}

/// The row writing the manifests, unless they already start Raffi. It has
/// no match text, so it stays on top whatever the query.
fn connect_item(setup: &HostSetup) -> Option<CommandItem> {
    let (title, subtitle, force, confirm) = match setup {
        HostSetup::Raffi => return None,
        HostSetup::None => (
            "Connect Raffi to your browsers",
            "Browser tabs are not set up. Press Enter to let browsers send their tabs to Raffi"
                .to_string(),
            false,
            None,
        ),
        HostSetup::MissingBinary(program) => (
            "Connect Raffi to your browsers",
            format!(
                "Browsers start {}, which no longer exists. Press Enter to fix it",
                program.display()
            ),
            false,
            None,
        ),
        HostSetup::Vicinae => (
            "Use Raffi instead of Vicinae for browser tabs",
            "Browsers send their tabs to Vicinae, which takes them back when it starts".to_string(),
            true,
            Some("replace the Vicinae browser connection"),
        ),
        HostSetup::Other(reason) => (
            "Use Raffi for browser tabs",
            format!("Another program receives the browser tabs: {reason}"),
            true,
            Some("replace the other program's browser connection"),
        ),
    };
    let mut item = setup_row(
        "setup:connect",
        title.to_string(),
        subtitle,
        "emblem-system",
        Some(CommandAction {
            action: DeferredAction::BrowserHostSetup { force },
            confirm: confirm.map(str::to_string),
            keep_open: true,
            progress: Some("Connecting browsers...".to_string()),
        }),
    );
    item.match_text.clear();
    let enter = if force { "Enter twice" } else { "Enter" };
    item.details = vec![(
        enter.to_string(),
        "Point the browsers at Raffi, as raffi --install-browser-host does".to_string(),
    )];
    Some(item)
}

/// Get the extension in a browser lacking it, or open a browser having it.
fn browser_item(browser: &InstalledBrowser) -> CommandItem {
    let program = browser.program.to_string_lossy().into_owned();
    let store = if browser.store_url == browser_host::FIREFOX_STORE_URL {
        "Firefox Add-ons"
    } else {
        "the Chrome Web Store"
    };
    let (identity, title, subtitle, argv, enter) = if browser.has_extension {
        (
            format!("setup:open:{}", browser.icon),
            format!("Open {}", browser.name),
            format!(
                "{} has the Vicinae extension and connects once running. Reload the extension if it does not",
                browser.name
            ),
            vec![program.clone()],
            format!("Start {}", browser.name),
        )
    } else {
        (
            format!("setup:store:{}", browser.icon),
            format!("Get the Vicinae extension for {}", browser.name),
            format!("Opens {store} in {}", browser.name),
            vec![program.clone(), browser.store_url.to_string()],
            format!("Open the extension page in {}", browser.name),
        )
    };
    let mut item = setup_row(
        &identity,
        title,
        subtitle,
        browser.icon,
        Some(CommandAction {
            action: DeferredAction::Command(argv),
            confirm: None,
            keep_open: false,
            progress: None,
        }),
    );
    item.icon_names.push(FALLBACK_ICON.to_string());
    item.details = vec![
        ("Enter".to_string(), enter),
        ("Program".to_string(), program),
    ];
    if !browser.has_extension {
        item.details
            .push(("URL".to_string(), browser.store_url.to_string()));
    }
    item
}

fn setup_row(
    identity: &str,
    title: String,
    subtitle: String,
    icon: &str,
    primary: Option<CommandAction>,
) -> CommandItem {
    CommandItem {
        match_text: title.clone(),
        subtitle,
        primary,
        badge: Some("Setup".to_string()),
        ..CommandItem::info(identity, title, icon)
    }
}

/// How the tab list is ordered, switched with Ctrl+Tab.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum View {
    /// Pinned tabs, then a section per browser.
    #[default]
    ByBrowser,
    /// Pinned tabs, then every other tab, the most recently used first.
    Recent,
}

impl View {
    pub(super) fn cycle(self) -> Self {
        match self {
            Self::ByBrowser => Self::Recent,
            Self::Recent => Self::ByBrowser,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ByBrowser => "By browser",
            Self::Recent => "Recent",
        }
    }
}

/// Pinned tabs first, then a section per browser, the one used last
/// first; within each, playing tabs first, then the most recently used.
/// `icons` holds the site icons by [`super::favicons::host_key`].
#[allow(dead_code)]
pub(super) fn tab_items(
    browsers: &[(PathBuf, Listing)],
    icons: &HashMap<String, PathBuf>,
) -> Result<Vec<CommandItem>, String> {
    tab_items_with(browsers, icons, View::ByBrowser, false, true)
}

type TabRow<'a> = (&'a Path, &'a Listing, &'a Tab);

/// The rows of `browsers` in `view` order. In both views the tab being
/// looked at, the newest one if it is shown in its window, comes last of
/// its list so that the previous tab comes first.
pub(super) fn tab_items_with(
    browsers: &[(PathBuf, Listing)],
    icons: &HashMap<String, PathBuf>,
    view: View,
    focus_keep_open: bool,
    close_keep_open: bool,
) -> Result<Vec<CommandItem>, String> {
    if browsers.is_empty() {
        return Err(NO_BROWSER.to_string());
    }
    let mut pinned: Vec<TabRow> = Vec::new();
    let mut sections: Vec<Vec<TabRow>> = Vec::new();
    for (socket, listing) in browsers {
        let (pins, rest): (Vec<_>, Vec<_>) = listing
            .tabs
            .iter()
            .map(|tab| (socket.as_path(), listing, tab))
            .partition(|(_, _, tab)| tab.pinned);
        pinned.extend(pins);
        if !rest.is_empty() {
            sections.push(rest);
        }
    }
    if pinned.is_empty() && sections.is_empty() {
        return Ok(vec![CommandItem::info(
            "empty",
            NO_TABS.to_string(),
            "dialog-information",
        )]);
    }
    let current = pinned
        .iter()
        .chain(sections.iter().flatten())
        .filter(|(_, _, tab)| tab.last_accessed > 0.0)
        .max_by(|(_, _, a), (_, _, b)| a.last_accessed.total_cmp(&b.last_accessed))
        .filter(|(_, _, tab)| tab.active)
        .map(|(socket, _, tab)| (*socket, tab.id));
    let is_current = |(socket, _, tab): &TabRow| current == Some((*socket, tab.id));
    let playing_first = view == View::ByBrowser;
    let by_use = |a: &TabRow, b: &TabRow| {
        is_current(a)
            .cmp(&is_current(b))
            .then(if playing_first {
                b.2.audible.cmp(&a.2.audible)
            } else {
                std::cmp::Ordering::Equal
            })
            .then(b.2.last_accessed.total_cmp(&a.2.last_accessed))
    };
    pinned.sort_by(by_use);
    if view == View::Recent {
        let mut rest: Vec<TabRow> = sections.into_iter().flatten().collect();
        rest.sort_by(by_use);
        return Ok(pinned
            .into_iter()
            .chain(rest)
            .map(|(socket, listing, tab)| {
                tab_item(
                    socket,
                    listing,
                    tab,
                    icons,
                    focus_keep_open,
                    close_keep_open,
                )
            })
            .collect());
    }
    let last_used = |section: &[TabRow]| {
        section
            .iter()
            .map(|(_, _, tab)| tab.last_accessed)
            .fold(f64::MIN, f64::max)
    };
    for section in &mut sections {
        section.sort_by(by_use);
    }
    sections.sort_by(|a, b| last_used(b).total_cmp(&last_used(a)));
    // A lone browser needs no heading unless pinned tabs sit above it.
    let headed = sections.len() > 1 || !pinned.is_empty();
    let pinned_group = RowGroup::new("Pinned", None);
    let rows = pinned
        .into_iter()
        .map(|row| (row, Some(pinned_group.clone())))
        .chain(sections.into_iter().flat_map(|section| {
            let browser = &section[0].1.browser;
            let group = headed.then(|| {
                let title = [&browser.name, &browser.id]
                    .into_iter()
                    .find(|name| !name.is_empty())
                    .map_or("Browser", String::as_str);
                RowGroup::new(title, Some(browser.id.as_str()).filter(|id| !id.is_empty()))
            });
            section.into_iter().map(move |row| (row, group.clone()))
        }));
    Ok(rows
        .map(|((socket, listing, tab), group)| CommandItem {
            group,
            ..tab_item(
                socket,
                listing,
                tab,
                icons,
                focus_keep_open,
                close_keep_open,
            )
        })
        .collect())
}

/// When a tab last had the focus, from its epoch milliseconds, as the
/// clipboard rows show times; None when the browser did not say.
fn last_used(last_accessed: f64, now: NaiveDateTime) -> Option<String> {
    if last_accessed <= 0.0 {
        return None;
    }
    let time = DateTime::from_timestamp_millis(last_accessed as i64)?
        .with_timezone(&Local)
        .naive_local();
    Some(super::clipboard::short_copy_time(time, now))
}

fn tab_item(
    socket: &Path,
    listing: &Listing,
    tab: &Tab,
    icons: &HashMap<String, PathBuf>,
    focus_keep_open: bool,
    close_keep_open: bool,
) -> CommandItem {
    let browser = &listing.browser;
    let title = if tab.title.is_empty() {
        tab.url.clone()
    } else {
        tab.title.clone()
    };
    let action = |operation, keep_open, progress: Option<&str>| CommandAction {
        action: DeferredAction::BrowserTab(TabAction {
            socket: socket.to_path_buf(),
            tab_id: tab.id,
            operation,
            browser_pid: browser.pid,
            browser_id: browser.id.clone(),
            title: tab.title.clone(),
            url: tab.url.clone(),
        }),
        confirm: None,
        keep_open,
        progress: progress.map(str::to_string),
    };
    let host = uri_host(&tab.url).unwrap_or_default();
    let subtitle = match (host.is_empty(), browser.name.is_empty()) {
        (false, false) => format!("{host} · {}", browser.name),
        (true, false) => browser.name.clone(),
        (_, true) => host,
    };
    let badge = if tab.muted {
        Some("Muted")
    } else if tab.audible {
        Some("Playing")
    } else {
        None
    };
    let host_id = socket
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let favicon = super::favicons::host_key(&tab.url)
        .and_then(|host| icons.get(&host))
        .map(|path| path.to_string_lossy().into_owned());
    let mut icon_names: Vec<String> = favicon
        .into_iter()
        .chain([browser.id.clone(), FALLBACK_ICON.to_string()])
        .collect();
    icon_names.retain(|name| !name.is_empty());
    let mut details = vec![
        ("URL".to_string(), tab.url.clone()),
        ("Browser".to_string(), browser.name.clone()),
    ];
    if tab.pinned {
        details.push(("Pinned".to_string(), "Yes".to_string()));
    }
    if let Some(time) = last_used(tab.last_accessed, Local::now().naive_local()) {
        details.push(("Last used".to_string(), time));
    }
    details.extend([
        ("Enter".to_string(), "Switch to the tab".to_string()),
        ("Ctrl+Enter".to_string(), "Close the tab".to_string()),
    ]);
    CommandItem {
        match_text: format!("{title} {}", tab.url),
        subtitle,
        icon_names,
        icon: None,
        identity: format!("{host_id}:{}", tab.id),
        unavailable: None,
        primary: Some(action(TabOperation::Focus, focus_keep_open, None)),
        secondary: Some(action(
            TabOperation::Close,
            close_keep_open,
            Some("Closing..."),
        )),
        details,
        badge: badge.map(str::to_string),
        domains: domains_of([tab.url.as_str()]),
        dimmed: false,
        sort_priority: 0,
        typed_only: false,
        close: None,
        group: None,
        pinned: tab.pinned,
        title,
    }
}

/// Switch to or close the tab. A switch then raises the browser window
/// through the compositor, since browsers often cannot raise themselves on
/// Wayland. Blocking: run it off the UI thread.
pub(super) fn run(action: &TabAction) -> Result<(), String> {
    let request = match action.operation {
        TabOperation::Focus => Request::Focus {
            tab_id: action.tab_id,
        },
        TabOperation::Close => Request::Close {
            tab_id: action.tab_id,
        },
    };
    browser_host::send_request(&action.socket, &request)?;
    if action.operation == TabOperation::Focus {
        if let Some(pid) = action.browser_pid {
            if let Err(error) = super::windows::raise_process_window(pid, &action.title) {
                crate::debug_log!("browser tabs: cannot raise the browser window: {error}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn fixture_items() -> Vec<CommandItem> {
    use crate::browser_host::Browser;
    let tab = |id, title: &str, url: &str, last_accessed| Tab {
        id,
        title: title.to_string(),
        url: url.to_string(),
        last_accessed,
        ..Tab::default()
    };
    tab_items(
        &[(
            PathBuf::from("/run/raffi/browsers/10.sock"),
            Listing {
                browser: Browser {
                    id: "firefox".to_string(),
                    name: "Firefox".to_string(),
                    pid: Some(9),
                },
                tabs: vec![
                    tab(1, "Raffi", "https://github.com/chmouel/raffi", 200.0),
                    tab(2, "Mail", "https://mail.example.com/inbox", 100.0),
                ],
            },
        )],
        &HashMap::new(),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_host::Browser;

    fn tab(id: i64, title: &str, url: &str, last_accessed: f64) -> Tab {
        Tab {
            id,
            title: title.to_string(),
            url: url.to_string(),
            last_accessed,
            ..Tab::default()
        }
    }

    fn listing(id: &str, name: &str, pid: Option<u32>, tabs: Vec<Tab>) -> Listing {
        Listing {
            browser: Browser {
                id: id.to_string(),
                name: name.to_string(),
                pid,
            },
            tabs,
        }
    }

    fn tab_action(item: &CommandItem, ctrl: bool) -> &TabAction {
        let action = if ctrl {
            item.secondary.as_ref()
        } else {
            item.primary.as_ref()
        };
        match &action.unwrap().action {
            DeferredAction::BrowserTab(action) => action,
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn test_tab_items_put_the_site_icon_before_the_browser_icon() {
        let browsers = vec![(
            PathBuf::from("/run/raffi/browsers/10.sock"),
            listing(
                "firefox",
                "Firefox",
                None,
                vec![
                    tab(1, "Docs", "https://docs.rs/iced", 2.0),
                    tab(2, "Blank", "about:blank", 1.0),
                ],
            ),
        )];
        let icons = HashMap::from([(
            "docs.rs".to_string(),
            PathBuf::from("/cache/raffi/favicons/docs.rs.png"),
        )]);
        let items = tab_items(&browsers, &icons).unwrap();
        assert_eq!(
            items[0].icon_names,
            vec![
                "/cache/raffi/favicons/docs.rs.png",
                "firefox",
                FALLBACK_ICON
            ]
        );
        assert_eq!(items[1].icon_names, vec!["firefox", FALLBACK_ICON]);
    }

    #[test]
    fn test_tab_items_group_by_browser_and_sort_playing_then_recent() {
        let mut playing = tab(7, "Music", "https://music.youtube.com/watch", 1.0);
        playing.audible = true;
        let mut radio = tab(3, "Radio", "https://radio.test", 50.0);
        radio.audible = true;
        let browsers = vec![
            (
                PathBuf::from("/run/raffi/browsers/20.sock"),
                listing("google-chrome", "Google Chrome", None, vec![playing]),
            ),
            (
                PathBuf::from("/run/raffi/browsers/10.sock"),
                listing(
                    "firefox",
                    "Firefox",
                    Some(9),
                    vec![
                        tab(1, "Old", "https://example.com/a", 100.0),
                        tab(2, "", "https://www.github.com/chmouel/raffi", 300.0),
                        radio,
                    ],
                ),
            ),
        ];
        let items = tab_items(&browsers, &HashMap::new()).unwrap();
        let rows: Vec<_> = items
            .iter()
            .map(|item| {
                let group = item.group.as_ref().unwrap();
                (
                    item.title.as_str(),
                    group.title.as_str(),
                    group.icon_name.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Radio", "Firefox", Some("firefox")),
                (
                    "https://www.github.com/chmouel/raffi",
                    "Firefox",
                    Some("firefox")
                ),
                ("Old", "Firefox", Some("firefox")),
                ("Music", "Google Chrome", Some("google-chrome")),
            ],
            "the browser used last comes first, playing tabs first in each"
        );

        let music = &items[3];
        assert_eq!(music.identity, "20:7");
        assert_eq!(music.subtitle, "music.youtube.com · Google Chrome");
        assert_eq!(music.badge.as_deref(), Some("Playing"));
        assert_eq!(music.icon_names, vec!["google-chrome", FALLBACK_ICON]);
        assert_eq!(music.domains, vec!["youtube.com"]);

        let github = &items[1];
        assert_eq!(github.identity, "10:2");
        assert_eq!(github.badge, None);
        assert!(github.match_text.contains("github.com/chmouel/raffi"));
        let focus = tab_action(github, false);
        assert_eq!(
            focus,
            &TabAction {
                socket: PathBuf::from("/run/raffi/browsers/10.sock"),
                tab_id: 2,
                operation: TabOperation::Focus,
                browser_pid: Some(9),
                browser_id: "firefox".to_string(),
                title: String::new(),
                url: "https://www.github.com/chmouel/raffi".to_string(),
            }
        );
        assert!(!github.primary.as_ref().unwrap().keep_open);
        let close = github.secondary.as_ref().unwrap();
        assert!(close.keep_open);
        assert_eq!(close.progress.as_deref(), Some("Closing..."));
        assert_eq!(tab_action(github, true).operation, TabOperation::Close);
    }

    #[test]
    fn test_tab_items_notices() {
        assert_eq!(tab_items(&[], &HashMap::new()).unwrap_err(), NO_BROWSER);
        let items = tab_items(
            &[(
                PathBuf::from("1.sock"),
                listing("firefox", "Firefox", None, Vec::new()),
            )],
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, NO_TABS);
        assert!(items[0].primary.is_none());
    }

    fn installed(name: &'static str, icon: &'static str, has_extension: bool) -> InstalledBrowser {
        InstalledBrowser {
            name,
            icon,
            program: PathBuf::from(format!("/usr/bin/{icon}")),
            store_url: browser_host::FIREFOX_STORE_URL,
            has_extension,
        }
    }

    fn argv(item: &CommandItem) -> Vec<String> {
        match &item.primary.as_ref().unwrap().action {
            DeferredAction::Command(argv) => argv.clone(),
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn test_setup_items_connect_then_browsers_then_guide() {
        let browsers = [
            installed("Firefox", "firefox", false),
            installed("Brave", "brave-browser", true),
        ];
        let items = setup_items(&HostSetup::None, &browsers);
        let titles: Vec<_> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Connect Raffi to your browsers",
                "Get the Vicinae extension for Firefox",
                "Open Brave",
                "Open the browser tabs setup guide",
            ]
        );

        let connect = items[0].primary.as_ref().unwrap();
        assert!(matches!(
            connect.action,
            DeferredAction::BrowserHostSetup { force: false }
        ));
        assert!(connect.keep_open && connect.confirm.is_none());
        assert!(items[0].match_text.is_empty());

        assert_eq!(
            argv(&items[1]),
            vec!["/usr/bin/firefox", browser_host::FIREFOX_STORE_URL]
        );
        assert_eq!(items[1].icon_names, vec!["firefox", FALLBACK_ICON]);
        assert_eq!(argv(&items[2]), vec!["/usr/bin/brave-browser"]);
        assert_eq!(argv(&items[3]), vec!["xdg-open", SETUP_GUIDE_URL]);
        assert!(items[1..].iter().all(|item| {
            let action = item.primary.as_ref().unwrap();
            !action.keep_open && !item.match_text.is_empty()
        }));
    }

    #[test]
    fn test_setup_items_per_host_setup() {
        let connect = |setup: &HostSetup| {
            setup_items(setup, &[])
                .into_iter()
                .find(|item| item.identity == "setup:connect")
        };
        assert!(connect(&HostSetup::Raffi).is_none());
        let items = setup_items(&HostSetup::Raffi, &[]);
        assert_eq!(items[0].title, NO_BROWSER);
        assert_eq!(items.len(), 2);

        let moved = connect(&HostSetup::MissingBinary("/old/raffi".into())).unwrap();
        assert!(moved
            .subtitle
            .contains("/old/raffi, which no longer exists"));
        assert!(moved.primary.unwrap().confirm.is_none());

        for setup in [
            HostSetup::Vicinae,
            HostSetup::Other("it starts /opt/x".into()),
        ] {
            let item = connect(&setup).unwrap();
            let action = item.primary.unwrap();
            assert!(matches!(
                action.action,
                DeferredAction::BrowserHostSetup { force: true }
            ));
            assert!(action.confirm.is_some(), "{setup:?} needs a second Enter");
        }
        assert!(connect(&HostSetup::Other("it starts /opt/x".into()))
            .unwrap()
            .subtitle
            .ends_with("it starts /opt/x"));
    }

    #[test]
    fn test_tab_badges() {
        let mut muted = tab(1, "a", "https://a.test", 0.0);
        muted.audible = true;
        muted.muted = true;
        let mut pinned = tab(2, "b", "https://b.test", 0.0);
        pinned.pinned = true;
        pinned.audible = true;
        let items = tab_items(
            &[(
                PathBuf::from("1.sock"),
                listing("", "", None, vec![muted, pinned]),
            )],
            &HashMap::new(),
        )
        .unwrap();
        assert!(items[0].pinned);
        assert_eq!(
            items[0].badge.as_deref(),
            Some("Playing"),
            "a pinned tab keeps its sound badge"
        );
        assert!(items[0]
            .details
            .contains(&("Pinned".to_string(), "Yes".to_string())));
        assert!(!items[1].pinned);
        assert_eq!(items[1].badge.as_deref(), Some("Muted"));
        assert_eq!(items[1].subtitle, "a.test");
        assert_eq!(items[1].icon_names, vec![FALLBACK_ICON]);
        assert_eq!(items[1].group.as_ref().unwrap().title, "Browser");
    }

    #[test]
    fn test_tab_items_list_pinned_tabs_first() {
        let pin = |id, title: &str, last_accessed| Tab {
            pinned: true,
            ..tab(id, title, "https://pin.test", last_accessed)
        };
        let firefox = || {
            (
                PathBuf::from("10.sock"),
                listing(
                    "firefox",
                    "Firefox",
                    None,
                    vec![
                        tab(1, "Docs", "https://docs.test", 50.0),
                        pin(2, "Mail", 10.0),
                    ],
                ),
            )
        };
        let chrome = (
            PathBuf::from("20.sock"),
            listing(
                "chromium",
                "Chromium",
                None,
                vec![
                    pin(3, "Chat", 20.0),
                    tab(4, "News", "https://news.test", 90.0),
                ],
            ),
        );
        let groups = |browsers: &[(PathBuf, Listing)]| -> Vec<(String, Option<String>)> {
            tab_items(browsers, &HashMap::new())
                .unwrap()
                .into_iter()
                .map(|item| (item.title, item.group.map(|group| group.title)))
                .collect()
        };
        let named = |title: &str, group: &str| (title.to_string(), Some(group.to_string()));

        assert_eq!(
            groups(&[firefox(), chrome]),
            vec![
                named("Chat", "Pinned"),
                named("Mail", "Pinned"),
                named("News", "Chromium"),
                named("Docs", "Firefox"),
            ]
        );
        assert_eq!(
            groups(&[firefox()]),
            vec![named("Mail", "Pinned"), named("Docs", "Firefox")],
            "a lone browser is headed below pinned tabs"
        );
        let unpinned = (
            PathBuf::from("10.sock"),
            listing(
                "firefox",
                "Firefox",
                None,
                vec![tab(1, "Docs", "https://docs.test", 50.0)],
            ),
        );
        assert_eq!(
            groups(&[unpinned]),
            vec![("Docs".to_string(), None)],
            "a lone browser without pinned tabs has no heading"
        );
    }

    #[test]
    fn test_tab_items_recent_view_and_current_tab_last() {
        let firefox = (
            PathBuf::from("10.sock"),
            listing(
                "firefox",
                "Firefox",
                None,
                vec![
                    Tab {
                        audible: true,
                        ..tab(1, "Radio", "https://radio.test", 10.0)
                    },
                    Tab {
                        active: true,
                        ..tab(2, "Current", "https://current.test", 90.0)
                    },
                    tab(3, "Docs", "https://docs.test", 50.0),
                ],
            ),
        );
        let chrome = (
            PathBuf::from("20.sock"),
            listing(
                "chromium",
                "Chromium",
                None,
                vec![
                    tab(4, "News", "https://news.test", 70.0),
                    Tab {
                        pinned: true,
                        ..tab(5, "Mail", "https://mail.test", 5.0)
                    },
                ],
            ),
        );
        let browsers = [firefox, chrome];
        let order = |view| -> Vec<(String, Option<String>)> {
            tab_items_with(&browsers, &HashMap::new(), view, false, true)
                .unwrap()
                .into_iter()
                .map(|item| (item.title, item.group.map(|group| group.title)))
                .collect()
        };
        let row = |title: &str, group: Option<&str>| (title.to_string(), group.map(str::to_string));

        assert_eq!(
            order(View::Recent),
            vec![
                row("Mail", None),
                row("News", None),
                row("Docs", None),
                row("Radio", None),
                row("Current", None),
            ],
            "pinned first, then by last use without sections, the current tab last"
        );
        assert_eq!(
            order(View::ByBrowser),
            vec![
                row("Mail", Some("Pinned")),
                row("Radio", Some("Firefox")),
                row("Docs", Some("Firefox")),
                row("Current", Some("Firefox")),
                row("News", Some("Chromium")),
            ]
        );
    }

    #[test]
    fn test_last_used() {
        let now = DateTime::from_timestamp_millis(1_000_000_000_000)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        let day_before = 1_000_000_000_000.0 - 86_400_000.0;
        assert_eq!(last_used(0.0, now), None);
        assert!(last_used(day_before, now)
            .unwrap()
            .starts_with("Yesterday "));
    }
}
