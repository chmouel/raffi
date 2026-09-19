use crate::AddonsConfig;

/// A single row of the keyword help list shown with `Ctrl+/`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HelpEntry {
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
            trigger: "~/".to_string(),
            title: "File browser".to_string(),
            usage: "~/ or /path/to/dir".to_string(),
            icon: None,
            completion: Some("~/".to_string()),
        });
    }

    for config in &addons.script_filters {
        entries.push(HelpEntry {
            trigger: config.keyword.clone(),
            title: config.name.clone(),
            usage: format!("{} <query>", config.keyword),
            icon: config.icon.clone(),
            completion: Some(format!("{} ", config.keyword)),
        });
    }

    for config in &addons.text_snippets {
        entries.push(HelpEntry {
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
            trigger: trigger.to_string(),
            title: "Emoji and icons".to_string(),
            usage: format!("{trigger} <name>"),
            icon: None,
            completion: Some(format!("{trigger} ")),
        });
    }

    for config in &addons.web_searches {
        entries.push(HelpEntry {
            trigger: config.keyword.clone(),
            title: config.name.clone(),
            usage: format!("{} <query>", config.keyword),
            icon: config.icon.clone(),
            completion: Some(format!("{} ", config.keyword)),
        });
    }

    if addons.calculator.enabled {
        entries.push(HelpEntry {
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
            trigger: trigger.to_string(),
            title: "Currency converter".to_string(),
            usage: "100 usd to eur".to_string(),
            icon: None,
            completion: Some(format!("{trigger} ")),
        });
    }

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
