use super::app::{route_query, QueryMode};
use super::state::LauncherApp;
use super::theme::ThemeColors;
use crate::ui::FontSizes;
use crate::{
    AddonsConfig, ScriptFilterConfig, SortMode, TextSnippet, TextSnippetSourceConfig,
    WebSearchConfig,
};
use std::sync::{Arc, Mutex};

fn test_app(addons: AddonsConfig) -> LauncherApp {
    let (app, _) = LauncherApp::new(
        Vec::new(),
        addons,
        true,
        Arc::new(Mutex::new(None)),
        None,
        ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None),
        10,
        FontSizes::default_sizes(),
        SortMode::Hybrid,
    );
    app
}

#[test]
fn test_route_query_file_browser_precedence() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "Files".into(),
        command: "files".into(),
        keyword: "/".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });

    assert!(matches!(
        route_query("~/dev", &addons),
        QueryMode::FileBrowser { .. }
    ));
}

#[test]
fn test_route_query_prefers_script_filter_over_text_snippet() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "Search".into(),
        command: "search".into(),
        keyword: "gh".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            name: "Issue".into(),
            value: "issue".into(),
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });

    assert!(matches!(
        route_query("gh rust", &addons),
        QueryMode::ScriptFilter {
            config_index: 0,
            ..
        }
    ));
}

#[test]
fn test_route_query_prefers_emoji_over_web_search() {
    let mut addons = AddonsConfig::default();
    addons.emoji.trigger = Some("emoji".into());
    addons.web_searches.push(WebSearchConfig {
        name: "Emoji Search".into(),
        keyword: "emoji".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
    });

    assert!(matches!(
        route_query("emoji smile", &addons),
        QueryMode::Emoji { .. }
    ));
}

#[test]
fn test_total_items_uses_grouped_state() {
    let mut app = test_app(AddonsConfig::default());
    app.filtered_configs = vec![0, 1];
    app.script_filter.loading = true;
    app.text_snippets.active = true;
    app.text_snippets.filtered = vec![0, 1, 2];
    app.currency.help = true;
    app.emoji.active = true;
    app.emoji.filtered = vec![0];

    assert_eq!(app.total_items(), 8);
}

#[test]
fn test_file_browser_clear_resets_browsing_results() {
    let mut app = test_app(AddonsConfig::default());
    app.file_browser.active = true;
    app.file_browser.current_dir = "/tmp".into();
    app.file_browser
        .entries
        .push(super::state::FileBrowserEntry {
            name: "a".into(),
            full_path: "/tmp/a".into(),
            is_dir: false,
        });
    app.file_browser.error = Some("boom".into());

    app.file_browser.clear();

    assert!(!app.file_browser.active);
    assert!(app.file_browser.current_dir.is_empty());
    assert!(app.file_browser.entries.is_empty());
    assert!(app.file_browser.error.is_none());
}

#[test]
fn test_font_sizes_default() {
    let fs = FontSizes::default_sizes();
    assert_eq!(fs.input, 24.0);
    assert_eq!(fs.item, 20.0);
    assert_eq!(fs.subtitle, 14.0);
    assert_eq!(fs.hint, 12.0);
    assert_eq!(fs.input_padding, 16.0);
    assert_eq!(fs.item_padding, 12.0);
    assert_eq!(fs.outer_padding, 20.0);
    assert_eq!(fs.scroll_top_padding, 8.0);
}

#[test]
fn test_font_sizes_from_base() {
    let fs = FontSizes::from_base(20.0);
    assert_eq!(fs.input, 24.0);
    assert_eq!(fs.item, 20.0);
    assert_eq!(fs.subtitle, 14.0);
    assert_eq!(fs.hint, 12.0);
    assert_eq!(fs.input_padding, 16.0);
    assert_eq!(fs.item_padding, 12.0);
    assert_eq!(fs.outer_padding, 20.0);
    assert_eq!(fs.scroll_top_padding, 8.0);

    let fs = FontSizes::from_base(40.0);
    assert_eq!(fs.input, 48.0);
    assert_eq!(fs.item, 40.0);
    assert_eq!(fs.subtitle, 28.0);
    assert_eq!(fs.hint, 24.0);
    assert_eq!(fs.input_padding, 32.0);
    assert_eq!(fs.item_padding, 24.0);
    assert_eq!(fs.outer_padding, 40.0);
    assert_eq!(fs.scroll_top_padding, 16.0);

    let fs = FontSizes::from_base(10.0);
    assert_eq!(fs.input, 12.0);
    assert_eq!(fs.item, 10.0);
    assert_eq!(fs.subtitle, 7.0);
    assert_eq!(fs.hint, 6.0);
    assert_eq!(fs.input_padding, 8.0);
    assert_eq!(fs.item_padding, 6.0);
    assert_eq!(fs.outer_padding, 10.0);
    assert_eq!(fs.scroll_top_padding, 4.0);
}

#[test]
fn test_mru_sort_key_frequency() {
    use super::support::{mru_sort_key, MruEntry};
    use std::collections::HashMap;

    let mut mru = HashMap::new();
    mru.insert(
        "A".to_string(),
        MruEntry {
            count: 10,
            last_used: 100,
        },
    );
    mru.insert(
        "B".to_string(),
        MruEntry {
            count: 5,
            last_used: 200,
        },
    );

    let key_a = mru_sort_key("A", &mru, &SortMode::Frequency, 10, 100, 200);
    let key_b = mru_sort_key("B", &mru, &SortMode::Frequency, 10, 100, 200);
    assert!(key_a > key_b, "A (count=10) should sort before B (count=5)");
}

#[test]
fn test_mru_sort_key_recency() {
    use super::support::{mru_sort_key, MruEntry};
    use std::collections::HashMap;

    let mut mru = HashMap::new();
    mru.insert(
        "A".to_string(),
        MruEntry {
            count: 10,
            last_used: 100,
        },
    );
    mru.insert(
        "B".to_string(),
        MruEntry {
            count: 5,
            last_used: 200,
        },
    );

    let key_a = mru_sort_key("A", &mru, &SortMode::Recency, 10, 100, 200);
    let key_b = mru_sort_key("B", &mru, &SortMode::Recency, 10, 100, 200);
    assert!(key_b > key_a, "B (ts=200) should sort before A (ts=100)");
}

#[test]
fn test_mru_sort_key_hybrid() {
    use super::support::{mru_sort_key, MruEntry};
    use std::collections::HashMap;

    let mut mru = HashMap::new();
    // A: high frequency, old timestamp
    mru.insert(
        "A".to_string(),
        MruEntry {
            count: 10,
            last_used: 100,
        },
    );
    // B: low frequency, recent timestamp
    mru.insert(
        "B".to_string(),
        MruEntry {
            count: 1,
            last_used: 200,
        },
    );

    let key_a = mru_sort_key("A", &mru, &SortMode::Hybrid, 10, 100, 200);
    let key_b = mru_sort_key("B", &mru, &SortMode::Hybrid, 10, 100, 200);
    // B: 0.4*(1/10) + 0.6*(100/100) = 0.04 + 0.6 = 0.64
    // A: 0.4*(10/10) + 0.6*(0/100) = 0.4 + 0.0 = 0.4
    assert!(
        key_b > key_a,
        "B (recent) should beat A (frequent) in hybrid"
    );
}

#[test]
fn test_mru_sort_key_unknown_entry() {
    use super::support::{mru_sort_key, MruEntry};
    use std::collections::HashMap;

    let mru: HashMap<String, MruEntry> = HashMap::new();
    let key = mru_sort_key("Unknown", &mru, &SortMode::Hybrid, 10, 100, 200);
    assert_eq!(key, 0);
}
