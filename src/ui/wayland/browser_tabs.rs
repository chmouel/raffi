use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::browser_host::{self, HostSetup, InstalledBrowser, Listing, Request, Tab};

use super::actions::DeferredAction;
use super::command_list::{domains_of, CommandAction, CommandItem};
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
pub(super) fn load_tabs() -> Result<Vec<CommandItem>, String> {
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
    tab_items(&browsers, &icons)
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

/// Playing tabs first, then the most recently used. `icons` holds the site
/// icons by [`super::favicons::host_key`].
pub(super) fn tab_items(
    browsers: &[(PathBuf, Listing)],
    icons: &HashMap<String, PathBuf>,
) -> Result<Vec<CommandItem>, String> {
    if browsers.is_empty() {
        return Err(NO_BROWSER.to_string());
    }
    let mut tabs: Vec<(&Path, &Listing, &Tab)> = browsers
        .iter()
        .flat_map(|(socket, listing)| {
            listing
                .tabs
                .iter()
                .map(move |tab| (socket.as_path(), listing, tab))
        })
        .collect();
    if tabs.is_empty() {
        return Ok(vec![CommandItem::info(
            "empty",
            NO_TABS.to_string(),
            "dialog-information",
        )]);
    }
    tabs.sort_by(|(_, _, a), (_, _, b)| {
        b.audible
            .cmp(&a.audible)
            .then(b.last_accessed.total_cmp(&a.last_accessed))
    });
    Ok(tabs
        .into_iter()
        .map(|(socket, listing, tab)| tab_item(socket, listing, tab, icons))
        .collect())
}

fn tab_item(
    socket: &Path,
    listing: &Listing,
    tab: &Tab,
    icons: &HashMap<String, PathBuf>,
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
    } else if tab.pinned {
        Some("Pinned")
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
    CommandItem {
        match_text: format!("{title} {}", tab.url),
        subtitle,
        icon_names,
        icon: None,
        identity: format!("{host_id}:{}", tab.id),
        unavailable: None,
        primary: Some(action(TabOperation::Focus, false, None)),
        secondary: Some(action(TabOperation::Close, true, Some("Closing..."))),
        details: vec![
            ("URL".to_string(), tab.url.clone()),
            ("Browser".to_string(), browser.name.clone()),
            ("Enter".to_string(), "Switch to the tab".to_string()),
            ("Ctrl+Enter".to_string(), "Close the tab".to_string()),
        ],
        badge: badge.map(str::to_string),
        domains: domains_of([tab.url.as_str()]),
        dimmed: false,
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
    fn test_tab_items_merge_browsers_and_sort_playing_then_recent() {
        let mut playing = tab(7, "Music", "https://music.youtube.com/watch", 1.0);
        playing.audible = true;
        let browsers = vec![
            (
                PathBuf::from("/run/raffi/browsers/10.sock"),
                listing(
                    "firefox",
                    "Firefox",
                    Some(9),
                    vec![
                        tab(1, "Old", "https://example.com/a", 100.0),
                        tab(2, "", "https://www.github.com/chmouel/raffi", 300.0),
                    ],
                ),
            ),
            (
                PathBuf::from("/run/raffi/browsers/20.sock"),
                listing("google-chrome", "Google Chrome", None, vec![playing]),
            ),
        ];
        let items = tab_items(&browsers, &HashMap::new()).unwrap();
        let titles: Vec<_> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Music", "https://www.github.com/chmouel/raffi", "Old"]
        );

        let music = &items[0];
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
        let items = tab_items(
            &[(
                PathBuf::from("1.sock"),
                listing("", "", None, vec![muted, pinned]),
            )],
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(items[0].badge.as_deref(), Some("Muted"));
        assert_eq!(items[1].badge.as_deref(), Some("Pinned"));
        assert_eq!(items[0].subtitle, "a.test");
        assert_eq!(items[0].icon_names, vec![FALLBACK_ICON]);
    }
}
