use super::app::{keyword_suggestions, route_query, QueryMode};
use super::help::{filter_help_entries, help_entries};
use super::state::{FallbackAction, KeywordSuggestion, LauncherApp, Message};
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
        Vec::new(),
    );
    app
}

fn test_app_with_fallbacks(addons: AddonsConfig, fallback_paths: Vec<String>) -> LauncherApp {
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
        fallback_paths,
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
        env: std::collections::HashMap::new(),
        min_query_length: None,
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
        env: std::collections::HashMap::new(),
        min_query_length: None,
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
fn test_route_query_requires_space_after_keyword() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            name: "Email".into(),
            value: "user@example.com".into(),
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });

    assert_eq!(route_query("sn", &addons), QueryMode::Standard);
    assert_eq!(route_query("  sn", &addons), QueryMode::Standard);
    assert_eq!(route_query("snip", &addons), QueryMode::Standard);

    assert_eq!(
        route_query("sn ", &addons),
        QueryMode::TextSnippet {
            config_index: 0,
            query: String::new()
        }
    );
    assert_eq!(
        route_query("  sn  email ", &addons),
        QueryMode::TextSnippet {
            config_index: 0,
            query: "email".into()
        }
    );
}

#[test]
fn test_keyword_suggestions_prefix_match() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: Some("snippet-icon".into()),
        snippets: None,
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
    });

    let suggestions = keyword_suggestions("s", &addons);
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].keyword, "sn");
    assert_eq!(suggestions[0].subtitle, "Browse Snippets");
    assert_eq!(suggestions[0].icon.as_deref(), Some("snippet-icon"));

    assert_eq!(keyword_suggestions("sn", &addons).len(), 1);
    assert_eq!(keyword_suggestions("SN", &addons).len(), 1);
    assert_eq!(
        keyword_suggestions("g", &addons)[0].subtitle,
        "Search GitHub"
    );
    assert!(keyword_suggestions("zz", &addons).is_empty());
}

#[test]
fn test_keyword_suggestions_stop_once_keyword_is_armed() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: None,
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });

    assert!(keyword_suggestions("", &addons).is_empty());
    assert!(keyword_suggestions("   ", &addons).is_empty());
    assert!(keyword_suggestions("sn ", &addons).is_empty());
    assert!(keyword_suggestions("sn email", &addons).is_empty());
    assert!(keyword_suggestions("/home", &addons).is_empty());
    assert!(keyword_suggestions("~/dev", &addons).is_empty());
}

#[test]
fn test_keyword_suggestions_follow_routing_precedence() {
    let mut addons = AddonsConfig::default();
    addons.emoji.trigger = Some("gh".into());
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
    });
    addons.script_filters.push(ScriptFilterConfig {
        name: "Gists".into(),
        command: "gists".into(),
        keyword: "gh".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
    });

    let subtitles: Vec<String> = keyword_suggestions("gh", &addons)
        .into_iter()
        .map(|suggestion| suggestion.subtitle)
        .collect();
    assert_eq!(
        subtitles,
        vec!["Search Gists", "Browse emoji and icons", "Search GitHub"]
    );
}

#[test]
fn test_keyword_suggestions_skip_disabled_emoji() {
    let mut addons = AddonsConfig::default();
    assert_eq!(keyword_suggestions("em", &addons).len(), 1);

    addons.emoji.enabled = false;
    assert!(keyword_suggestions("em", &addons).is_empty());
}

#[test]
fn test_total_items_counts_keyword_suggestions() {
    let mut app = test_app(AddonsConfig::default());
    app.filtered_configs = vec![0, 1];
    app.keyword_suggestions = vec![KeywordSuggestion {
        keyword: "sn".into(),
        subtitle: "Browse Snippets".into(),
        icon: None,
    }];

    assert_eq!(app.total_items(), 3);
}

#[test]
fn test_keyword_suggestions_suppress_fallbacks() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Google".into(),
        keyword: "g".into(),
        url: "https://google.com/search?q={query}".into(),
        icon: None,
    });
    let mut app = test_app_with_fallbacks(addons, vec!["addons.web_searches.Google".into()]);
    app.search_query = "sn".into();
    assert_eq!(app.fallback_count(), 1);

    app.keyword_suggestions = vec![KeywordSuggestion {
        keyword: "sn".into(),
        subtitle: "Browse Snippets".into(),
        icon: None,
    }];
    assert_eq!(app.fallback_count(), 0);
}

fn snippet_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            name: "Email".into(),
            value: "user@example.com".into(),
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });
    addons
}

#[test]
fn test_tab_completes_selected_keyword_suggestion() {
    let mut app = test_app(snippet_addons());
    let _ = app.update(Message::SearchChanged("sn".into()));
    assert_eq!(app.keyword_suggestions.len(), 1);
    assert_eq!(app.selected_index, 0);

    let _ = app.update(Message::TabComplete);

    assert_eq!(app.search_query, "sn ");
    assert!(app.keyword_suggestions.is_empty());
    assert!(app.text_snippets.active);
}

#[test]
fn test_tab_leaves_launcher_selection_alone() {
    let mut app = test_app(snippet_addons());
    let _ = app.update(Message::SearchChanged("sn".into()));
    assert_eq!(app.keyword_suggestions.len(), 1);

    app.selected_index = app.keyword_suggestions.len();
    let _ = app.update(Message::TabComplete);

    assert_eq!(app.search_query, "sn");
    assert_eq!(app.keyword_suggestions.len(), 1);
    assert!(!app.text_snippets.active);
}

#[test]
fn test_tab_without_suggestions_is_a_no_op() {
    let mut app = test_app(snippet_addons());
    let _ = app.update(Message::SearchChanged("zzz".into()));
    assert!(app.keyword_suggestions.is_empty());
    assert!(!app.file_browser.active);

    let _ = app.update(Message::TabComplete);

    assert_eq!(app.search_query, "zzz");
}

#[test]
fn test_enter_on_keyword_suggestion_completes_it() {
    let mut app = test_app(snippet_addons());
    let _ = app.update(Message::SearchChanged("s".into()));
    assert_eq!(app.keyword_suggestions.len(), 1);

    let _ = app.update(Message::Submit);

    assert_eq!(app.search_query, "sn ");
    assert!(app.keyword_suggestions.is_empty());
    assert!(app.text_snippets.active);
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

#[test]
fn test_resolve_fallbacks_web_search() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Google".into(),
        keyword: "g".into(),
        url: "https://google.com/search?q={query}".into(),
        icon: None,
    });

    let app = test_app_with_fallbacks(addons, vec!["addons.web_searches.Google".into()]);

    assert_eq!(app.fallbacks.len(), 1);
    assert_eq!(app.fallbacks[0].name, "Google");
    assert!(matches!(
        app.fallbacks[0].action,
        FallbackAction::WebSearch { .. }
    ));
}

#[test]
fn test_resolve_fallbacks_unknown_path_ignored() {
    let addons = AddonsConfig::default();
    let app = test_app_with_fallbacks(addons, vec!["addons.web_searches.NonExistent".into()]);
    assert!(app.fallbacks.is_empty());
}

#[test]
fn test_resolve_fallbacks_script_filter() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "Brave".into(),
        command: "brave-search".into(),
        keyword: "brave".into(),
        icon: None,
        args: vec!["--json".into()],
        action: Some("open".into()),
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
    });

    let app = test_app_with_fallbacks(addons, vec!["addons.script_filters.Brave".into()]);

    assert_eq!(app.fallbacks.len(), 1);
    assert_eq!(app.fallbacks[0].name, "Brave");
    assert!(matches!(
        app.fallbacks[0].action,
        FallbackAction::ScriptFilter {
            ref command,
            ref action,
            ..
        } if command == "brave-search" && action.as_deref() == Some("open")
    ));
}

#[test]
fn test_resolve_fallbacks_empty_when_no_paths() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Google".into(),
        keyword: "g".into(),
        url: "https://google.com/search?q={query}".into(),
        icon: None,
    });

    let app = test_app(addons);
    assert!(app.fallbacks.is_empty());
}

fn help_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "Gists".into(),
        command: "gists".into(),
        keyword: "gist".into(),
        icon: Some("gist-icon".into()),
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
    });
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            name: "Email".into(),
            value: "user@example.com".into(),
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
    });
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
    });
    addons
}

#[test]
fn test_help_entries_cover_every_enabled_addon_in_routing_order() {
    let entries = help_entries(&help_addons());
    let triggers: Vec<&str> = entries.iter().map(|entry| entry.trigger.as_str()).collect();

    assert_eq!(
        triggers,
        vec!["~/", "gist", "sn", "emoji", "gh", "math", "$"]
    );
    assert_eq!(entries[1].icon.as_deref(), Some("gist-icon"));
    assert_eq!(entries[1].completion.as_deref(), Some("gist "));
    assert_eq!(entries[0].completion.as_deref(), Some("~/"));
    assert_eq!(entries[5].completion, None);
}

#[test]
fn test_help_entries_skip_disabled_addons() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.calculator.enabled = false;
    addons.currency.enabled = false;
    addons.file_browser.enabled = false;

    assert!(help_entries(&addons).is_empty());
}

#[test]
fn test_help_entries_use_custom_triggers() {
    let mut addons = AddonsConfig::default();
    addons.emoji.trigger = Some("ic".into());
    addons.currency.trigger = Some("cur".into());

    let entries = help_entries(&addons);
    assert!(entries
        .iter()
        .any(|entry| entry.trigger == "ic" && entry.completion.as_deref() == Some("ic ")));
    assert!(entries
        .iter()
        .any(|entry| entry.trigger == "cur" && entry.completion.as_deref() == Some("cur ")));
}

#[test]
fn test_filter_help_entries_matches_trigger_title_and_usage() {
    let entries = help_entries(&help_addons());

    assert_eq!(filter_help_entries(&entries, "").len(), entries.len());
    assert_eq!(filter_help_entries(&entries, "   ").len(), entries.len());

    let by_title: Vec<&str> = filter_help_entries(&entries, "SNIPPETS")
        .into_iter()
        .map(|index| entries[index].trigger.as_str())
        .collect();
    assert_eq!(by_title, vec!["sn"]);

    let by_trigger: Vec<&str> = filter_help_entries(&entries, "gist")
        .into_iter()
        .map(|index| entries[index].trigger.as_str())
        .collect();
    assert_eq!(by_trigger, vec!["gist"]);

    let by_usage: Vec<&str> = filter_help_entries(&entries, "usd")
        .into_iter()
        .map(|index| entries[index].trigger.as_str())
        .collect();
    assert_eq!(by_usage, vec!["$"]);

    assert!(filter_help_entries(&entries, "zzz").is_empty());
}

#[test]
fn test_help_replaces_launcher_entries() {
    let mut app = test_app(help_addons());
    app.filtered_configs = vec![0, 1, 2];
    app.search_query = "gist".into();

    let _ = app.update(Message::ToggleHelp);

    assert!(app.view.help_active);
    assert!(app.filtered_configs.is_empty());
    assert!(app.keyword_suggestions.is_empty());
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.fallback_count(), 0);
}

#[test]
fn test_help_list_filters_while_typing() {
    let mut app = test_app(help_addons());
    let _ = app.update(Message::ToggleHelp);
    assert_eq!(app.total_items(), app.help_entries.len());

    let _ = app.update(Message::SearchChanged("gist".into()));
    assert_eq!(app.total_items(), 1);
    assert!(app.filtered_configs.is_empty());
    assert!(app.calculator_result.is_none());
}

#[test]
fn test_help_selection_arms_keyword() {
    let mut app = test_app(help_addons());
    let _ = app.update(Message::ToggleHelp);
    let _ = app.update(Message::SearchChanged("sn".into()));
    assert_eq!(app.total_items(), 1);

    let _ = app.update(Message::Submit);

    assert!(!app.view.help_active);
    assert_eq!(app.search_query, "sn ");
    assert!(app.text_snippets.active);
}

#[test]
fn test_help_selection_of_calculator_row_is_a_no_op() {
    let mut app = test_app(help_addons());
    let _ = app.update(Message::ToggleHelp);
    let _ = app.update(Message::SearchChanged("math".into()));
    assert_eq!(app.total_items(), 1);

    let _ = app.update(Message::Submit);

    assert!(app.view.help_active);
    assert_eq!(app.search_query, "math");
}

#[test]
fn test_help_closes_on_toggle_and_cancel() {
    let mut app = test_app(help_addons());
    app.search_query = "gist ".into();

    let _ = app.update(Message::ToggleHelp);
    assert!(app.view.help_active);
    let _ = app.update(Message::ToggleHelp);
    assert!(!app.view.help_active);
    assert!(app.help_filtered.is_empty());
    assert!(app.script_filter.loading || app.script_filter.results.is_some());

    let _ = app.update(Message::ToggleHelp);
    assert!(app.view.help_active);
    let _ = app.update(Message::Cancel);
    assert!(!app.view.help_active);
}
