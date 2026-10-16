use super::preview::describe_file_action;
use crate::AddonsConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HelpSection {
    KeywordsAndActions,
    KeyboardShortcuts,
}

impl HelpSection {
    pub(super) fn heading(self) -> &'static str {
        match self {
            Self::KeywordsAndActions => "Keywords and actions",
            Self::KeyboardShortcuts => "Keyboard shortcuts",
        }
    }
}

/// A single row of the keyword help list shown with `Ctrl+/`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HelpEntry {
    pub section: HelpSection,
    pub trigger: String,
    pub title: String,
    pub usage: String,
    pub icon: Option<String>,
    /// Text placed in the search input when the row is selected. `None` marks
    /// an informational row that cannot be armed, such as the calculator.
    pub completion: Option<String>,
}

/// Build the help rows for every enabled addon.
///
/// The order mirrors [`super::app::route_query`] precedence so the help list
/// reflects which trigger actually wins when two of them overlap.
pub(super) fn help_entries(addons: &AddonsConfig) -> Vec<HelpEntry> {
    let mut entries = Vec::new();

    if addons.file_browser.enabled {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: "~/".to_string(),
            title: "File browser".to_string(),
            usage: "~/ or /path/to/dir".to_string(),
            icon: None,
            completion: Some("~/".to_string()),
        });
    }

    for config in &addons.script_filters {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: config.keyword.clone(),
            title: config.name.clone(),
            usage: format!("{} <query>", config.keyword),
            icon: config.icon.clone(),
            completion: Some(format!("{} ", config.keyword)),
        });
    }

    for config in &addons.text_snippets {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: config.keyword.clone(),
            title: config.name.clone(),
            usage: format!("{} <filter>", config.keyword),
            icon: config.icon.clone(),
            completion: Some(format!("{} ", config.keyword)),
        });
    }

    if addons.emoji.enabled {
        let trigger = addons.emoji.trigger.as_deref().unwrap_or("emoji");
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Emoji and icons".to_string(),
            usage: format!("{trigger} <name>"),
            icon: None,
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.clipboard.enabled {
        let trigger = addons.clipboard.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Clipboard history".to_string(),
            usage: format!("{trigger} <text>"),
            icon: Some(super::app::CLIPBOARD_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
        let trigger = addons.clipboard.image_trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Clipboard images".to_string(),
            usage: format!("{trigger} <text>"),
            icon: Some(super::app::CLIPBOARD_IMAGES_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    for config in &addons.web_searches {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: config.keyword.clone(),
            title: config.name.clone(),
            usage: format!("{} <query>", config.keyword),
            icon: config.icon.clone(),
            completion: Some(format!("{} ", config.keyword)),
        });
    }

    if addons.system.enabled {
        let trigger = addons.system.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "System commands".to_string(),
            usage: format!("{trigger} <lock|suspend|log out|reboot|shut down>"),
            icon: Some(super::app::SYSTEM_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.windows.enabled {
        let trigger = addons.windows.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Window switcher".to_string(),
            usage: format!("{trigger} <title or app>, Ctrl+Enter brings it here"),
            icon: Some(super::app::WINDOWS_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.media.enabled {
        let trigger = addons.media.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Media controls".to_string(),
            usage: format!("{trigger} <action or player>, Enter plays, pauses or skips"),
            icon: Some(super::app::MEDIA_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.audio.enabled {
        let trigger = addons.audio.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Audio devices".to_string(),
            usage: format!("{trigger} <device>, Enter sets the default, Ctrl+Enter mutes"),
            icon: Some(super::app::AUDIO_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.kill.enabled {
        let trigger = addons.kill.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Kill process".to_string(),
            usage: format!("{trigger} <name>, Enter twice sends SIGTERM, Ctrl+Enter twice SIGKILL"),
            icon: Some(super::app::KILL_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.bluetooth.enabled {
        let trigger = addons.bluetooth.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Bluetooth".to_string(),
            usage: format!("{trigger} <device>, Enter connects, or disconnects when pressed twice"),
            icon: Some(super::app::BLUETOOTH_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.devtools.enabled {
        let trigger = addons.devtools.trigger();
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Developer tools".to_string(),
            usage: format!("{trigger} <text> to encode, decode, hash or reformat it"),
            icon: Some(super::app::DEVTOOLS_ICON.to_string()),
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.calculator.enabled {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: "math".to_string(),
            title: "Calculator".to_string(),
            usage: "2+2*3 or (10/4)^2".to_string(),
            icon: None,
            completion: None,
        });
    }

    if addons.currency.enabled {
        let trigger = addons.currency.trigger.as_deref().unwrap_or("$");
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: trigger.to_string(),
            title: "Currency converter".to_string(),
            usage: "100 usd to eur".to_string(),
            icon: None,
            completion: Some(format!("{trigger} ")),
        });
    }

    if addons.units.enabled {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: "units".to_string(),
            title: "Unit converter".to_string(),
            usage: "10 km to mi or 70f to c".to_string(),
            icon: None,
            completion: None,
        });
    }

    if addons.color.enabled {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: "color".to_string(),
            title: "Color converter".to_string(),
            usage: "#ff8800, rgb(255 136 0) or hsl(32 100% 50%)".to_string(),
            icon: None,
            completion: None,
        });
    }

    if addons.world_clock.enabled {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: "time".to_string(),
            title: "World clock".to_string(),
            usage: "time tokyo or 15:00 paris to nyc".to_string(),
            icon: None,
            completion: None,
        });
    }

    if addons.devtools.enabled {
        entries.push(HelpEntry {
            section: HelpSection::KeywordsAndActions,
            trigger: "uuid/pw".to_string(),
            title: "UUID and password generator".to_string(),
            usage: "uuid, pw or pw 32".to_string(),
            icon: None,
            completion: None,
        });
    }

    entries.extend(keyboard_shortcut_entries(addons));

    entries
}

/// Informational rows listing the global keyboard shortcuts, shown after the
/// keyword rows. They cannot be armed, so Enter and Tab do nothing on them.
fn keyboard_shortcut_entries(addons: &AddonsConfig) -> Vec<HelpEntry> {
    let shortcut = |trigger: &str, title: &str, usage: &str| HelpEntry {
        section: HelpSection::KeyboardShortcuts,
        trigger: trigger.to_string(),
        title: title.to_string(),
        usage: usage.to_string(),
        icon: None,
        completion: None,
    };

    let mut entries = vec![
        shortcut(
            "↑/↓",
            "Navigate results",
            "Moves the selection through the results",
        ),
        shortcut(
            "Ctrl+J",
            "Scroll preview down",
            "Scrolls the right-hand preview pane",
        ),
        shortcut(
            "Ctrl+K",
            "Scroll preview up",
            "Scrolls the right-hand preview pane",
        ),
        shortcut(
            "Ctrl+P",
            "Toggle preview pane",
            "Shows or hides the right-hand preview",
        ),
    ];
    let file_browser = &addons.file_browser;
    if file_browser.enabled {
        entries.extend([
            shortcut(
                "Ctrl+H",
                "Toggle hidden files",
                "File browser: show or hide dotfiles",
            ),
            shortcut(
                "Ctrl+O",
                "Open in file manager",
                "File browser: opens the current directory with the entry selected",
            ),
            shortcut(
                "Ctrl+S",
                "Cycle sort order",
                "File browser: name, modified date, created date",
            ),
        ]);
    }
    if addons.clipboard.enabled {
        entries.push(shortcut(
            "Ctrl+R",
            "Refresh clipboard history",
            "Clipboard history: reload the list from the clipboard manager",
        ));
        entries.push(shortcut(
            "Ctrl+I",
            "Show only images",
            "Clipboard history: switch between all items and images only",
        ));
    }
    entries.extend([
        shortcut("Alt+P/N", "Search history", "Previous / next query"),
        shortcut(
            "Tab",
            "Complete selection",
            "Completes the keyword or file browser path",
        ),
    ]);
    if file_browser.enabled && file_browser.backspace_updir {
        entries.push(shortcut(
            "Backspace",
            "Parent directory",
            "File browser: on a trailing / goes up one directory",
        ));
    }
    entries.extend([
        shortcut("Ctrl+/", "Keyword and shortcut help", "Opens this list"),
        shortcut("Esc", "Close help or quit", "Closes help, otherwise exits"),
    ]);
    let (enter_usage, ctrl_enter_usage) = if file_browser.enabled {
        (
            format!(
                "Launches, arms or activates the selection. File browser files: {}",
                describe_file_action(file_browser.action())
            ),
            format!(
                "Runs the addon's secondary action. File browser: {}",
                describe_file_action(file_browser.secondary_action())
            ),
        )
    } else {
        (
            "Launches, arms or activates the selection".to_string(),
            "Runs the addon's secondary action".to_string(),
        )
    };
    entries.extend([
        shortcut("Enter", "Use selected entry", &enter_usage),
        shortcut("Ctrl+Enter", "Secondary action", &ctrl_enter_usage),
    ]);
    entries
}

/// Return the indices of the help rows matching `query`.
///
/// Matching is a case-insensitive substring test over the trigger, title and
/// usage text. An empty query keeps every row, in declaration order.
pub(super) fn filter_help_entries(entries: &[HelpEntry], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (0..entries.len()).collect();
    }

    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.trigger.to_lowercase().contains(&needle)
                || entry.title.to_lowercase().contains(&needle)
                || entry.usage.to_lowercase().contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}
