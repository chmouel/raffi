use super::actions::DeferredAction;
use super::app::{
    classify_query, fido_restart, fido_updir, keyword_suggestions, loggable_query, reveal_offset,
    route_query, tilde_path, QueryMode,
};
use super::command_list::CommandListAddon;
use super::help::{filter_help_entries, help_entries, search_tips, HelpSection};
use super::hyprland::HyprAction;
use super::placeholders::Expanded;
use super::preview::{
    format_byte_size, inspect_path, PreviewBody, MAX_FILE_PREVIEW_BYTES, MAX_FILE_PREVIEW_LINES,
};
use super::state::{
    ClipboardData, CurrencyConversionRequest, CurrencyResult, EmojiEntry, FallbackAction,
    KeywordSuggestion, LauncherApp, Message, ScriptFilterResult,
};
use super::system::system_entries;
use super::theme::ThemeColors;
use super::types::{
    InlineProvider, InlineResult, InlineRow, ScriptFilterItem, ScriptFilterItemText,
};
use super::windows::Compositor;
use crate::ui::FontSizes;
use crate::{
    AddonsConfig, RaffiConfig, ScriptFilterConfig, SortMode, TextSnippet, TextSnippetSourceConfig,
    WebSearchConfig,
};
use std::sync::{Arc, Mutex};

fn test_app(addons: AddonsConfig) -> LauncherApp {
    test_app_with_fallbacks(addons, Vec::new())
}

fn test_app_with_fallbacks(addons: AddonsConfig, fallback_paths: Vec<String>) -> LauncherApp {
    let (mut app, _) = LauncherApp::new(
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
    // Keep the user's snippet usage cache out of tests, both ways.
    app.text_snippets.mru_map.clear();
    app.text_snippets.mru_path = None;
    app.vault.usage.clear();
    app.vault.usage_path = None;
    app.program_exists = |_| true;
    app.terminal_tab = |_, _| None;
    app
}

#[test]
fn test_script_filter_tree_guides_follow_sibling_branches() {
    let items = tree_items(&[0, 1, 2, 2, 1, 0, 1, 2]);
    let guides = super::view::tree_guide_segments(&items);
    let segments = |index: usize| guides[index].clone();
    use super::view::TreeGuideSegments as Segment;

    assert!(segments(0).is_empty());
    assert_eq!(
        segments(1),
        vec![Segment {
            above: true,
            below: true,
            elbow: true,
        }]
    );
    assert_eq!(
        segments(2),
        vec![
            Segment {
                above: true,
                below: true,
                elbow: false,
            },
            Segment {
                above: true,
                below: true,
                elbow: true,
            },
        ]
    );
    assert_eq!(
        segments(3),
        vec![
            Segment {
                above: true,
                below: true,
                elbow: false,
            },
            Segment {
                above: true,
                below: false,
                elbow: true,
            },
        ]
    );
    assert_eq!(
        segments(4),
        vec![Segment {
            above: true,
            below: false,
            elbow: true,
        }]
    );
    assert!(segments(5).is_empty());
    assert_eq!(
        segments(7),
        vec![
            Segment {
                above: false,
                below: false,
                elbow: false,
            },
            Segment {
                above: true,
                below: false,
                elbow: true,
            },
        ]
    );
}

#[test]
fn test_script_filter_tree_guides_keep_raw_depths_and_visible_boundaries() {
    let items = tree_items(&[0, 10, 11, 10, u32::MAX]);
    let guides = super::view::tree_guide_segments(&items);
    assert!(guides[1][9].elbow);
    assert!(guides[1][9].below);
    assert!(!guides[2][9].elbow);
    assert!(guides[2][9].below);
    assert!(!guides[3][9].below);
    assert!(!guides[4].iter().any(|segment| segment.elbow));

    let truncated = super::view::tree_guide_segments(&items[..3]);
    assert!(!truncated[1][9].below);
    assert!(!truncated[2][9].below);

    let irregular = super::view::tree_guide_segments(&tree_items(&[2, 0, 2]));
    assert!(!irregular[0]
        .iter()
        .any(|segment| segment.above || segment.below || segment.elbow));
    assert!(!irregular[2][0].above);
    assert!(irregular[2][1].elbow);
}

fn tree_items(depths: &[u32]) -> Vec<ScriptFilterItem> {
    depths
        .iter()
        .map(|&indent| {
            let mut item = plain_item("tree");
            item.indent = indent;
            item
        })
        .collect()
}

#[test]
fn test_script_filter_tree_guides_large_flat_subtree() {
    let mut depths = vec![2; 10_000];
    depths[0] = 0;
    depths[1] = 1;
    let guides = super::view::tree_guide_segments(&tree_items(&depths));
    assert_eq!(guides.len(), depths.len());
    assert!(guides[2][1].below);
    assert!(!guides.last().unwrap()[1].below);
}

#[cfg(target_os = "linux")]
#[test]
fn test_preview_script_filter_reads_virtual_file_with_zero_reported_size() {
    let path = std::path::Path::new("/proc/cpuinfo");
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() || meta.len() != 0 {
        return;
    }

    let mut app = test_app(AddonsConfig::default());
    app.script_filter.results = Some(ScriptFilterResult {
        default_icon: None,
        items: vec![ScriptFilterItem {
            title: "CPU info".into(),
            valid: true,
            subtitle: None,
            arg: None,
            indent: 0,
            icon: None,
            preview: None,
            quicklookurl: Some("file:///proc/cpuinfo".into()),
            variables: Default::default(),
            text: None,
        }],
    });

    let preview = app
        .resolve_selected_preview()
        .expect("expected Script Filter preview");
    assert!(preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Type" && v == "Text file"));
    assert!(preview.bodies.iter().any(|body| matches!(
        body,
        PreviewBody::Text {
            heading: Some(heading),
            content,
            ..
        } if heading == "Contents" && !content.is_empty()
    )));
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
        dynamic_preview: None,
        ..Default::default()
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
        dynamic_preview: None,
        ..Default::default()
    });
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            icon: None,
            name: "Issue".into(),
            value: "issue".into(),
            keyword: None,
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
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
        ..Default::default()
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
            icon: None,
            name: "Email".into(),
            value: "user@example.com".into(),
            keyword: None,
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
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
    addons.system.enabled = false;
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
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
    });
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
        ..Default::default()
    });

    // By default a short prefix suggests nothing, but a full keyword does.
    assert!(keyword_suggestions("s", &addons).is_empty());
    assert_eq!(keyword_suggestions("sn", &addons).len(), 1);

    addons.keyword_min_chars = Some(1);
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
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
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
        ..Default::default()
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
        dynamic_preview: None,
        ..Default::default()
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
    assert_eq!(keyword_suggestions("emo", &addons).len(), 1);

    addons.emoji.enabled = false;
    assert!(keyword_suggestions("emo", &addons).is_empty());
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
        ..Default::default()
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

fn suggested_keywords(query: &str, addons: &AddonsConfig) -> Vec<String> {
    keyword_suggestions(query, addons)
        .into_iter()
        .map(|suggestion| suggestion.keyword)
        .collect()
}

#[test]
fn test_keyword_suggestions_match_feature_words() {
    let mut addons = AddonsConfig::default();
    assert_eq!(suggested_keywords("volume", &addons), vec!["audio"]);
    assert_eq!(suggested_keywords("Lock", &addons), vec!["sys"]);
    assert_eq!(suggested_keywords("files", &addons), vec!["~/"]);
    // Feature words only match whole.
    assert!(suggested_keywords("volum", &addons).is_empty());
    assert!(suggested_keywords("vol", &addons).is_empty());

    addons.pass.enabled = false;
    assert_eq!(suggested_keywords("password", &addons), vec!["bw"]);
    addons.bitwarden.enabled = false;
    assert!(suggested_keywords("password", &addons).is_empty());
}

#[test]
fn test_feature_word_suggestions_keep_fallbacks() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Google".into(),
        keyword: "g".into(),
        url: "https://google.com/search?q={query}".into(),
        icon: None,
        ..Default::default()
    });
    let mut app = test_app_with_fallbacks(addons, vec!["addons.web_searches.Google".into()]);
    let _ = app.update(Message::SearchChanged("volume".into()));
    assert_eq!(app.keyword_suggestions.len(), 1);
    assert_eq!(app.fallback_count(), 1);
}

#[test]
fn test_unmatched_query_offers_help_rows() {
    let mut app = test_app(help_addons());
    let _ = app.update(Message::SearchChanged("switcher".into()));
    let keywords: Vec<&str> = app
        .keyword_suggestions
        .iter()
        .map(|suggestion| suggestion.keyword.as_str())
        .collect();
    assert_eq!(keywords, vec!["win"]);

    let _ = app.update(Message::SearchChanged("zzzz".into()));
    assert!(app.keyword_suggestions.is_empty());
}

#[test]
fn test_help_rows_not_offered_when_a_launcher_matches() {
    let mut app = test_app(help_addons());
    app.configs = vec![RaffiConfig {
        description: Some("Switcher".into()),
        binary: Some("switcher".into()),
        ..Default::default()
    }];
    let _ = app.update(Message::SearchChanged("switcher".into()));
    assert_eq!(app.filtered_configs, vec![0]);
    assert!(app.keyword_suggestions.is_empty());
}

#[test]
fn test_question_mark_opens_help() {
    let mut app = test_app(help_addons());
    let _ = app.update(Message::SearchChanged("?".into()));
    assert!(app.view.help_active);
    assert!(app.search_query.is_empty());

    let _ = app.update(Message::ToggleHelp);
    assert!(!app.view.help_active);
    assert!(app.search_query.is_empty());
}

#[test]
fn test_question_mark_keyword_wins_over_help() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Ask".into(),
        keyword: "?".into(),
        url: "https://example.com/?q={query}".into(),
        icon: None,
        ..Default::default()
    });
    let mut app = test_app(addons);
    let _ = app.update(Message::SearchChanged("?".into()));
    assert!(!app.view.help_active);
    assert_eq!(app.search_query, "?");
}

#[test]
fn test_search_tips_follow_enabled_addons() {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = Some(true);
    let tips = search_tips(&help_entries(&addons), &addons, true);
    assert!(tips
        .iter()
        .any(|tip| tip.contains("tip: ? for all keywords")));
    assert!(tips
        .iter()
        .any(|tip| tip.contains("tip: clip for Clipboard history")));

    addons.clipboard.enabled = Some(false);
    addons.calculator.enabled = false;
    let tips = search_tips(&help_entries(&addons), &addons, false);
    assert!(!tips
        .iter()
        .any(|tip| tip.contains("clip for") || tip.contains("?") || tip.contains("2+2")));
}

fn snippet_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            icon: None,
            name: "Email".into(),
            value: "user@example.com".into(),
            keyword: None,
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
    });
    addons
}

fn snippet_source(
    name: &str,
    keyword: &str,
    snippets: Vec<TextSnippet>,
) -> TextSnippetSourceConfig {
    TextSnippetSourceConfig {
        name: name.into(),
        keyword: keyword.into(),
        icon: None,
        snippets: Some(snippets),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
    }
}

fn snippet(name: &str, value: &str) -> TextSnippet {
    TextSnippet {
        icon: None,
        name: name.into(),
        value: value.into(),
        keyword: None,
    }
}

fn open_snippets(app: &mut LauncherApp, query: &str) {
    let _ = app.update(Message::SearchChanged(query.into()));
    assert!(app.text_snippets.active);
}

#[test]
fn test_snippet_previews_use_fetched_clipboard_and_ignore_stale_results() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(snippet_source(
        "Snippets",
        "sn",
        vec![
            snippet("Quote", "> {clipboard}"),
            snippet("Signed", "{snippet:Name}!"),
            snippet("Name", "Raffi"),
            snippet("Older", "was {clipboard:2}"),
        ],
    ));
    let mut app = test_app(addons);
    open_snippets(&mut app, "sn ");

    assert!(app.text_snippets.clipboard.fetching);
    assert_eq!(app.text_snippets.previews[0], None);
    assert_eq!(app.text_snippets.previews[1].as_deref(), Some("Raffi!"));
    assert_eq!(app.text_snippets.previews[3], None);

    let fetched = |current: &str, older: &str| {
        let mut data = ClipboardData {
            current: Some(Ok(current.into())),
            ..Default::default()
        };
        data.history.insert(2, Ok(older.into()));
        data
    };
    let session = app.text_snippets.session;
    let _ = app.update(Message::TextSnippetClipboard(
        session.wrapping_sub(1),
        fetched("stale", "stale"),
    ));
    assert_eq!(app.text_snippets.previews[0], None);
    assert_eq!(app.text_snippets.previews[3], None);

    let _ = app.update(Message::TextSnippetClipboard(
        session,
        fetched("copied", "before"),
    ));
    assert_eq!(app.text_snippets.previews[0].as_deref(), Some("> copied"));
    assert_eq!(app.text_snippets.previews[3].as_deref(), Some("was before"));

    // Typing more keeps the fetched clipboard instead of reading it again.
    let _ = app.update(Message::SearchChanged("sn quo".into()));
    assert!(!app.text_snippets.clipboard.fetching);
    assert_eq!(
        app.text_snippets.clipboard.data,
        fetched("copied", "before")
    );

    // Snippet text is prose, drawn in the preview font rather than monospace.
    let preview = app.resolve_selected_preview().unwrap();
    assert!(matches!(
        preview.bodies.as_slice(),
        [
            PreviewBody::Prose { content, .. },
            PreviewBody::Prose { content: raw, .. },
        ] if content == "> copied" && raw == "> {clipboard}"
    ));
}

#[test]
fn test_snippet_expansion_error_keeps_launcher_open() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(snippet_source(
        "Snippets",
        "sn",
        vec![snippet("Bad", "{date:%Q}")],
    ));
    let mut app = test_app(addons);
    open_snippets(&mut app, "sn ");

    let _ = app.update(Message::TextSnippetSelected(0));
    let session = app.text_snippets.session;
    assert!(app.text_snippets.pending.is_some());

    let _ = app.update(Message::TextSnippetExpanded(
        session,
        Err("bad format".into()),
    ));

    assert!(app.text_snippets.active);
    assert!(app.text_snippets.pending.is_none());
    assert_eq!(app.text_snippets.error.as_deref(), Some("bad format"));
    // The error row is not selectable.
    assert_eq!(app.total_items(), 1);
}

#[test]
fn test_hypr_paste_preparation_tracks_selection_and_reports_errors() {
    for placeholders in [false, true] {
        let mut source = snippet_source("Snippets", "sn", vec![snippet("Hi", "hello")]);
        source.action = Some("hypr_paste".into());
        source.secondary_action = Some("hypr_paste".into());
        source.placeholders = placeholders;
        source.restore_clipboard = true;
        let mut addons = AddonsConfig::default();
        addons.text_snippets.push(source);
        let mut app = test_app(addons);
        app.hyprland = Some(Ok(super::hyprland::Hyprland::test_target()));
        open_snippets(&mut app, "sn ");
        assert!(app.text_snippets.restore_clipboard);
        app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
        let _ = app.update(Message::TextSnippetSelected(0));
        let session = app.text_snippets.session;
        if placeholders {
            app.view.current_modifiers = iced::keyboard::Modifiers::empty();
            let _ = app.update(Message::TextSnippetExpanded(
                session,
                Ok(Expanded {
                    text: "hello".into(),
                    cursor_moves: None,
                }),
            ));
        }
        assert!(app.text_snippets.pending.as_ref().unwrap().preparing_paste);
        let _ = app.update(Message::TextSnippetSelected(0));
        assert!(app.text_snippets.pending.as_ref().unwrap().preparing_paste);
        let _ = app.update(Message::HyprPastePrepared(
            session.wrapping_sub(1),
            Ok(super::hyprland::Hyprland::test_target()),
        ));
        assert!(app.deferred_action.lock().unwrap().is_none());
        assert!(app.text_snippets.pending.is_some());
        let _ = app.update(Message::HyprPastePrepared(
            session,
            Err("wl-copy failed".into()),
        ));
        assert_eq!(app.text_snippets.error.as_deref(), Some("wl-copy failed"));
        assert!(app.text_snippets.active);
        assert!(app.text_snippets.pending.is_none());
        assert!(app.deferred_action.lock().unwrap().is_none());

        let _ = app.update(Message::TextSnippetSelected(0));
        if placeholders {
            let _ = app.update(Message::TextSnippetExpanded(
                session,
                Ok(Expanded {
                    text: "hello".into(),
                    cursor_moves: None,
                }),
            ));
        }
        let _ = app.update(Message::HyprPastePrepared(
            session,
            Ok(super::hyprland::Hyprland::test_target()),
        ));
        assert!(app.deferred_action.lock().unwrap().is_some());
        assert!(app.text_snippets.pending.is_none());
    }
}

#[test]
fn test_hypr_paste_is_default_action_inside_hyprland() {
    use super::hyprland::{default_to_hypr_paste, Hyprland};
    let sources = || {
        let mut explicit = snippet_source("Explicit", "ex", vec![]);
        explicit.action = Some("copy".into());
        vec![snippet_source("Default", "de", vec![]), explicit]
    };
    let actions = |sources: &[TextSnippetSourceConfig]| {
        sources
            .iter()
            .map(|source| source.action.clone())
            .collect::<Vec<_>>()
    };
    let unchanged = vec![None, Some("copy".to_string())];

    // Inside Hyprland with a paste target: only unset actions change.
    let mut list = sources();
    let target = default_to_hypr_paste(&mut list, true, || Ok(Hyprland::test_target()));
    assert!(matches!(target, Some(Ok(_))));
    assert_eq!(
        actions(&list),
        vec![Some("hypr_paste".to_string()), Some("copy".to_string())]
    );

    // No paste target: keep the copy default and do not report an error.
    let mut list = sources();
    assert!(default_to_hypr_paste(&mut list, true, || Err("no window".into())).is_none());
    assert_eq!(actions(&list), unchanged);

    // Outside Hyprland: nothing is captured or changed.
    let mut list = sources();
    assert!(default_to_hypr_paste(&mut list, false, || unreachable!()).is_none());
    assert_eq!(actions(&list), unchanged);

    // An explicit hypr_paste still reports capture errors.
    let mut list = sources();
    list[1].secondary_action = Some("hypr_paste".into());
    let target = default_to_hypr_paste(&mut list, true, || Err("no window".into()));
    assert!(matches!(target, Some(Err(_))));
    assert_eq!(actions(&list), unchanged);
}

#[test]
fn test_hypr_paste_missing_session_and_late_results_do_not_queue_paste() {
    let mut source = snippet_source("Snippets", "sn", vec![snippet("Hi", "hello")]);
    source.action = Some("hypr_paste".into());
    source.placeholders = false;
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(source);
    let mut app = test_app(addons);
    open_snippets(&mut app, "sn ");
    let _ = app.update(Message::TextSnippetSelected(0));
    assert!(app
        .text_snippets
        .error
        .as_ref()
        .unwrap()
        .contains("Hyprland session"));
    assert!(app.text_snippets.pending.is_none());

    app.hyprland = Some(Ok(super::hyprland::Hyprland::test_target()));
    let _ = app.update(Message::TextSnippetSelected(0));
    let session = app.text_snippets.session;
    let _ = app.update(Message::SearchChanged("other ".into()));
    let _ = app.update(Message::HyprPastePrepared(
        session,
        Ok(super::hyprland::Hyprland::test_target()),
    ));
    assert!(app.deferred_action.lock().unwrap().is_none());
}

#[test]
fn test_snippet_results_from_a_previous_source_are_ignored() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons
        .text_snippets
        .push(snippet_source("First", "aa", vec![snippet("One", "1")]));
    addons
        .text_snippets
        .push(snippet_source("Second", "bb", vec![snippet("Two", "2")]));
    let mut app = test_app(addons);
    open_snippets(&mut app, "aa ");
    let _ = app.update(Message::TextSnippetSelected(0));
    let old_session = app.text_snippets.session;

    open_snippets(&mut app, "bb ");
    assert_ne!(app.text_snippets.session, old_session);
    assert!(app.text_snippets.pending.is_none());

    let _ = app.update(Message::TextSnippetExpanded(
        old_session,
        Err("late".into()),
    ));
    assert!(app.text_snippets.error.is_none());

    let _ = app.update(Message::SearchChanged("zzz".into()));
    assert!(!app.text_snippets.active);
    let _ = app.update(Message::TextSnippetExpanded(
        old_session,
        Err("late".into()),
    ));
    assert!(app.text_snippets.error.is_none());
}

#[test]
fn test_snippet_command_result_after_leaving_snippets_is_ignored() {
    let mut source = snippet_source("Snippets", "sn", Vec::new());
    source.snippets = None;
    source.command = Some("snippets-command".into());
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(source);
    let mut app = test_app(addons);
    open_snippets(&mut app, "sn ");
    assert!(app.text_snippets.loading);
    let generation = app.text_snippets.generation;

    let _ = app.update(Message::SearchChanged("zzz".into()));
    assert!(!app.text_snippets.active);

    let _ = app.update(Message::TextSnippetCommandResult(
        generation,
        Ok(vec![snippet("Quote", "> {clipboard}")]),
    ));
    assert!(app.text_snippets.items.is_empty());
    assert!(app.text_snippets.filtered.is_empty());
    assert!(!app.text_snippets.clipboard.fetching);
}

#[test]
fn test_snippet_command_result_from_previous_source_is_ignored_by_inline_source() {
    let mut command_source = snippet_source("Command", "cc", Vec::new());
    command_source.snippets = None;
    command_source.command = Some("snippets-command".into());
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(command_source);
    addons
        .text_snippets
        .push(snippet_source("Inline", "ii", vec![snippet("Hi", "hi")]));
    let mut app = test_app(addons);
    open_snippets(&mut app, "cc ");
    assert!(app.text_snippets.loading);
    let old_generation = app.text_snippets.generation;

    let _ = app.update(Message::SearchChanged("zzz".into()));
    assert!(!app.text_snippets.active);
    open_snippets(&mut app, "ii ");
    assert!(app.text_snippets.allow_shell);

    let _ = app.update(Message::TextSnippetCommandResult(
        old_generation,
        Ok(vec![snippet("Quote", "> {clipboard} {shell:id}")]),
    ));
    assert_eq!(app.text_snippets.items, vec![snippet("Hi", "hi")]);
    assert_eq!(app.text_snippets.filtered, vec![0]);
    assert!(!app.text_snippets.clipboard.fetching);
}

#[test]
fn test_snippet_selection_captures_action_and_ignores_repeats() {
    let dir = std::env::temp_dir().join(format!("raffi-snippet-app-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out");

    let mut source = snippet_source("Snippets", "sn", vec![snippet("Hi", "hi {cursor}there")]);
    source.action = Some(format!(
        "printf '%s' \"$RAFFI_VALUE\" > '{}'",
        out.display()
    ));
    source.secondary_action = Some("false".into());
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(source);
    let mut app = test_app(addons);
    // Selecting exits and saves the query; keep the real history cache intact.
    app.history.max_items = 0;
    open_snippets(&mut app, "sn ");

    let _ = app.update(Message::TextSnippetSelected(0));
    let session = app.text_snippets.session;
    let captured = app.text_snippets.pending.clone().unwrap();

    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let _ = app.update(Message::TextSnippetSelected(0));
    assert_eq!(app.text_snippets.pending.as_ref(), Some(&captured));

    let _ = app.update(Message::TextSnippetExpanded(
        session,
        Ok(Expanded {
            text: "hi there".into(),
            cursor_moves: Some(5),
        }),
    ));
    assert!(app.text_snippets.pending.is_none());
    assert!(app.text_snippets.error.is_none());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline && std::fs::read_to_string(&out).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "hi there");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_snippet_selection_moves_snippet_to_top() {
    let mut source = snippet_source(
        "Snippets",
        "sn",
        vec![snippet("First", "1"), snippet("Second", "2")],
    );
    source.placeholders = false;
    source.action = Some("true".into());
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(source);
    let mut app = test_app(addons);
    app.history.max_items = 0;
    app.sort_mode = SortMode::Recency;

    open_snippets(&mut app, "sn ");
    assert_eq!(app.text_snippets.filtered, vec![0, 1]);
    let _ = app.update(Message::TextSnippetSelected(1));
    assert_eq!(app.text_snippets.mru_map["sn:Second"].count, 1);

    open_snippets(&mut app, "sn x");
    open_snippets(&mut app, "sn ");
    assert_eq!(app.text_snippets.filtered, vec![1, 0]);
}

#[test]
fn test_snippet_shell_policy_follows_source() {
    let mut command_source = snippet_source("Command", "cc", Vec::new());
    command_source.snippets = None;
    command_source.command = Some("true".into());
    let mut disabled = snippet_source("Plain", "pp", vec![snippet("Today", "{date}")]);
    disabled.placeholders = false;
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(snippet_source(
        "Inline",
        "ii",
        vec![snippet("Today", "{date}")],
    ));
    addons.text_snippets.push(command_source);
    addons.text_snippets.push(disabled);
    let mut app = test_app(addons);

    open_snippets(&mut app, "ii ");
    assert!(app.text_snippets.allow_shell);
    assert_ne!(app.text_snippets.previews[0].as_deref(), Some("{date}"));

    open_snippets(&mut app, "cc ");
    assert!(!app.text_snippets.allow_shell);

    open_snippets(&mut app, "pp ");
    assert!(!app.text_snippets.allow_shell);
    assert!(app.text_snippets.previews.is_empty());
}

#[test]
fn test_snippet_directory_reads_metadata_headers() {
    let dir = std::env::temp_dir().join(format!("raffi-snippet-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("sig.snippet"),
        "Signature\nkeyword: !sig\nicon: ✍️\n---\n-- me",
    )
    .unwrap();
    std::fs::write(dir.join("plain.snippet"), "Address\n---\n1 Main St").unwrap();

    let mut source = snippet_source("Dir", "dd", Vec::new());
    source.snippets = None;
    source.directory = Some(dir.display().to_string());
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(source);
    let mut app = test_app(addons);
    open_snippets(&mut app, "dd !sig");

    let first = app.text_snippets.filtered[0];
    assert_eq!(app.text_snippets.items[first].name, "Signature");
    assert_eq!(app.text_snippets.items[first].icon.as_deref(), Some("✍️"));
    assert_eq!(
        app.text_snippets.items[first].keyword.as_deref(),
        Some("!sig")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_snippet_icons_override_source_and_respect_no_icons() {
    use super::snippets::SnippetIcon;
    let mut app = test_app(AddonsConfig::default());
    app.no_icons = false;
    let path = std::env::temp_dir().join(format!("raffi-snippet-icon-{}.svg", std::process::id()));
    std::fs::write(&path, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
    let path = path.to_string_lossy().into_owned();
    app.icon_map.insert("mail".into(), path.clone());
    app.text_snippets.icon = Some("mail".into());
    let mut item = snippet("Address", "body");
    assert_eq!(
        app.snippet_icon(&item),
        Some(SnippetIcon::File(path.clone()))
    );
    item.icon = Some("🏠".into());
    assert_eq!(
        app.snippet_icon(&item),
        Some(SnippetIcon::Glyph("🏠".into()))
    );
    item.icon = Some(path.clone());
    assert_eq!(
        app.snippet_icon(&item),
        Some(SnippetIcon::File(path.clone()))
    );
    for missing in ["missing-icon", "", "/missing/icon.png"] {
        item.icon = Some(missing.into());
        assert_eq!(
            app.snippet_icon(&item),
            Some(SnippetIcon::File(path.clone()))
        );
    }
    app.no_icons = true;
    assert_eq!(app.snippet_icon(&item), None);
    item.icon = Some("🧑‍💻".into());
    assert_eq!(app.snippet_icon(&item), None);
    app.no_icons = false;
    assert_eq!(
        app.snippet_icon(&item),
        Some(SnippetIcon::Glyph("🧑‍💻".into()))
    );
    item.icon = Some("1️⃣".into());
    assert_eq!(
        app.snippet_icon(&item),
        Some(SnippetIcon::Glyph("1️⃣".into()))
    );
    std::fs::remove_file(path).unwrap();
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
    let _ = app.update(Message::SearchChanged("sn".into()));
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
    app.file_browser.pressed_row = Some(0);
    app.file_browser.dragging = true;

    app.file_browser.clear();

    assert!(!app.file_browser.active);
    assert!(app.file_browser.current_dir.is_empty());
    assert!(app.file_browser.entries.is_empty());
    assert!(app.file_browser.error.is_none());
    assert!(app.file_browser.pressed_row.is_none());
    assert!(!app.file_browser.dragging);
}

/// A tiny browser fixture with two directories; activating a directory only
/// rewrites the query, so tests can exercise click routing without spawning
/// anything.
fn browser_app_with_two_dirs() -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.keyword_suggestions.clear();
    app.file_browser.active = true;
    for name in ["alpha", "beta"] {
        app.file_browser
            .entries
            .push(super::state::FileBrowserEntry {
                name: name.into(),
                full_path: format!("/tmp/{name}"),
                is_dir: true,
            });
    }
    app
}

#[test]
fn test_file_browser_row_press_then_release_activates() {
    let mut app = browser_app_with_two_dirs();

    let _ = app.update(Message::FileBrowserRowPressed {
        index: 1,
        path: "/tmp/beta".into(),
    });
    assert_eq!(app.file_browser.pressed_row, Some(1));

    let _ = app.update(Message::FileBrowserRowReleased(1));
    assert_eq!(app.selected_index, 1);
    // Activating a directory rewrites the query into it.
    assert_eq!(app.search_query, "/tmp/beta/");
    assert!(app.file_browser.pressed_row.is_none());
}

#[test]
fn test_file_browser_row_release_requires_matching_press() {
    let mut app = browser_app_with_two_dirs();
    app.selected_index = 0;

    // A release without a press on that row (drag takeover, press elsewhere)
    // must not activate anything.
    let _ = app.update(Message::FileBrowserRowReleased(1));
    assert_eq!(app.selected_index, 0);
    assert_eq!(app.search_query, "");

    // Pressing one row and releasing over another is not a click either.
    let _ = app.update(Message::FileBrowserRowPressed {
        index: 0,
        path: "/tmp/alpha".into(),
    });
    let _ = app.update(Message::FileBrowserRowReleased(1));
    assert_eq!(app.selected_index, 0);
    assert_eq!(app.search_query, "");
    assert!(app.file_browser.pressed_row.is_none());
}

#[test]
fn test_mouse_release_resets_row_press_tracking() {
    let mut app = browser_app_with_two_dirs();

    let _ = app.update(Message::FileBrowserRowPressed {
        index: 0,
        path: "/tmp/alpha".into(),
    });
    // A release outside the browser rows still ends the press.
    let _ = app.update(Message::MouseReleased);
    assert!(app.file_browser.pressed_row.is_none());

    let _ = app.update(Message::FileBrowserRowReleased(0));
    assert_eq!(app.search_query, "");
}

#[test]
fn test_file_browser_runs_configured_actions() {
    let dir = std::env::temp_dir().join(format!("raffi-fb-action-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let primary = dir.join("primary");
    let secondary = dir.join("secondary");
    let write_to =
        |out: &std::path::Path| format!("printf '%s' \"$RAFFI_VALUE\" > '{}'", out.display());

    let mut addons = AddonsConfig::default();
    addons.file_browser.action = Some(write_to(&primary));
    addons.file_browser.secondary_action = Some(write_to(&secondary));
    let mut app = test_app(addons);
    app.history.max_items = 0;
    app.keyword_suggestions.clear();
    app.file_browser.active = true;
    for (name, is_dir) in [("dir", true), ("file.txt", false)] {
        app.file_browser
            .entries
            .push(super::state::FileBrowserEntry {
                name: name.into(),
                full_path: format!("/tmp/{name}"),
                is_dir,
            });
    }

    // Enter on a directory keeps navigating instead of running the action.
    let _ = app.update(Message::FileBrowserItemSelected(0));
    assert_eq!(app.search_query, "/tmp/dir/");

    let _ = app.update(Message::FileBrowserItemSelected(1));
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let _ = app.update(Message::FileBrowserItemSelected(0));

    let read = |out: &std::path::Path| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && std::fs::read_to_string(out).is_err() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::read_to_string(out).unwrap()
    };
    assert_eq!(read(&primary), "/tmp/file.txt");
    assert_eq!(read(&secondary), "/tmp/dir");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_file_browser_opens_current_dir_in_configured_file_manager() {
    let dir = std::env::temp_dir().join(format!("raffi-fb-fm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("out");

    let mut addons = AddonsConfig::default();
    addons.file_browser.file_manager = Some(format!(
        "sh -c 'printf \"%s\" \"$1\" > {}' fm",
        out.display()
    ));
    let mut app = test_app(addons);
    app.history.max_items = 0;

    // Outside the file browser the shortcut does nothing.
    let _ = app.update(Message::FileBrowserOpenFileManager);
    assert!(!out.exists());

    let browsed = format!("{}/", dir.display());
    app.file_browser.active = true;
    app.file_browser.current_dir = browsed.clone();
    let _ = app.update(Message::FileBrowserOpenFileManager);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline && std::fs::read_to_string(&out).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), browsed);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_file_browser_shortcuts_route_to_expected_messages() {
    let control = iced::keyboard::Modifiers::CTRL;
    let control_alt = control | iced::keyboard::Modifiers::ALT;
    assert!(matches!(
        super::view::file_browser_shortcut_message("o", control),
        Some(Message::FileBrowserOpenSelected)
    ));
    assert!(matches!(
        super::view::file_browser_shortcut_message("b", control),
        Some(Message::FileBrowserOpenFileManager)
    ));
    assert!(matches!(
        super::view::file_browser_shortcut_message("y", control),
        Some(Message::FileBrowserCopyPath)
    ));
    assert!(matches!(
        super::view::file_browser_shortcut_message("y", control_alt),
        Some(Message::FileBrowserCopyImage)
    ));
    assert!(
        super::view::file_browser_shortcut_message("y", iced::keyboard::Modifiers::ALT).is_none()
    );
}

#[test]
fn test_file_browser_copy_image_rejects_unsupported_selection() {
    let mut app = test_app(AddonsConfig::default());
    app.file_browser.active = true;
    app.file_browser
        .entries
        .push(super::state::FileBrowserEntry {
            name: "notes.txt".into(),
            full_path: "/tmp/notes.txt".into(),
            is_dir: false,
        });

    let _ = app.update(Message::FileBrowserCopyImage);

    assert_eq!(
        app.file_browser.error.as_deref(),
        Some("Cannot copy image: selected entry is not a supported image")
    );
}

#[test]
fn test_image_mime_type_uses_supported_file_extensions() {
    assert_eq!(
        super::preview::image_mime_type(std::path::Path::new("image.PNG")),
        Some("image/png")
    );
    assert_eq!(
        super::preview::image_mime_type(std::path::Path::new("image.tiff")),
        Some("image/tiff")
    );
    assert_eq!(
        super::preview::image_mime_type(std::path::Path::new("notes.txt")),
        None
    );
    assert!(!super::preview::is_previewable_image_path(
        std::path::Path::new("image.tiff")
    ));
}

#[test]
fn test_file_browser_cycle_sort_reorders_entries() {
    let dir = std::env::temp_dir().join(format!("raffi-fb-sort-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base = std::time::SystemTime::now();
    for (name, age) in [("a.txt", 300), ("b.txt", 10)] {
        let file = std::fs::File::create(dir.join(name)).unwrap();
        file.set_modified(base - std::time::Duration::from_secs(age))
            .unwrap();
    }

    let mut addons = AddonsConfig::default();
    addons.file_browser.sort = crate::FileBrowserSort::Name;
    let mut app = test_app(addons);
    assert_eq!(app.file_browser.sort, crate::FileBrowserSort::Name);

    // Outside the file browser the shortcut is ignored.
    let _ = app.update(Message::FileBrowserCycleSort);
    assert_eq!(app.file_browser.sort, crate::FileBrowserSort::Name);

    app.file_browser.active = true;
    app.file_browser.current_dir = format!("{}/", dir.display());
    let names = |app: &LauncherApp| -> Vec<String> {
        app.file_browser
            .all_entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    };

    let _ = app.update(Message::FileBrowserCycleSort);
    assert_eq!(app.file_browser.sort, crate::FileBrowserSort::Modified);
    assert_eq!(names(&app), vec!["b.txt", "a.txt"]);

    let _ = app.update(Message::FileBrowserCycleSort);
    assert_eq!(app.file_browser.sort, crate::FileBrowserSort::Created);
    let _ = app.update(Message::FileBrowserCycleSort);
    assert_eq!(app.file_browser.sort, crate::FileBrowserSort::Name);
    assert_eq!(names(&app), vec!["a.txt", "b.txt"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_file_browser_cycle_show_filters_entries() {
    let dir = std::env::temp_dir().join(format!("raffi-fb-show-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::create_dir_all(dir.join("nest").join("deep")).unwrap();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("sub").join("inner.txt"), "").unwrap();
    let root = format!("{}/", dir.display());
    let names = |app: &LauncherApp| -> Vec<String> {
        app.file_browser
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    };

    let mut app = test_app(AddonsConfig::default());
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::All);

    let _ = app.update(Message::SearchChanged(root.clone()));
    assert_eq!(names(&app), vec!["nest", "sub", "a.txt"]);

    let _ = app.update(Message::CycleView(false));
    let _ = app.update(Message::SearchChanged(root.clone()));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Files);
    assert_eq!(names(&app), vec!["a.txt"]);
    assert!(app.file_browser.error.is_none());

    // Hidden directories can still be entered by typing or Tab-completing.
    let _ = app.update(Message::SearchChanged(format!("{root}su")));
    assert!(names(&app).is_empty());
    assert!(app.file_browser.error.is_none());
    let _ = app.update(Message::TabComplete);
    assert_eq!(app.search_query, format!("{root}sub/"));
    let _ = app.update(Message::SearchChanged(format!("{root}sub/")));
    assert_eq!(names(&app), vec!["inner.txt"]);

    let _ = app.update(Message::SearchChanged(format!("{root}nest/")));
    assert!(names(&app).is_empty());
    assert_eq!(
        app.file_browser.error.as_deref(),
        Some("No files here (Ctrl+Tab for directories)")
    );

    let _ = app.update(Message::CycleView(false));
    let _ = app.update(Message::SearchChanged(format!("{root}sub/")));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Directories);
    assert!(names(&app).is_empty());
    assert_eq!(
        app.file_browser.error.as_deref(),
        Some("No directories here (Ctrl+Tab to show all)")
    );

    // The mode survives leaving and re-entering the browser.
    let _ = app.update(Message::SearchChanged(String::new()));
    assert!(!app.file_browser.active);
    let _ = app.update(Message::SearchChanged(root.clone()));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Directories);
    assert_eq!(names(&app), vec!["nest", "sub"]);

    let _ = app.update(Message::CycleView(false));
    let _ = app.update(Message::SearchChanged(root.clone()));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::All);
    assert_eq!(names(&app), vec!["nest", "sub", "a.txt"]);
    assert_eq!(app.cycle_view_label().as_deref(), Some("All"));

    // Shift goes back.
    let _ = app.update(Message::CycleView(true));
    let _ = app.update(Message::SearchChanged(root));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Directories);
    assert_eq!(app.cycle_view_label().as_deref(), Some("Directories"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_drag_events_drive_browser_dragging_state() {
    let mut app = browser_app_with_two_dirs();
    app.file_browser.pressed_row = Some(0);

    let _ = app.update(Message::DragEvent(super::dnd::DragEvent::Started));
    assert!(app.file_browser.dragging);
    // The compositor consumed the pointer: press tracking resets.
    assert!(app.file_browser.pressed_row.is_none());

    // A drag ending over the origin window must not launch the file: the
    // release finds no matching press, so the row does not activate.
    // Seeded non-zero so both assertions discriminate under regression
    // (activation would set selected_index back to 0 and rewrite the query).
    app.selected_index = 1;
    let _ = app.update(Message::FileBrowserRowReleased(0));
    assert_eq!(app.selected_index, 1);
    assert_eq!(app.search_query, "");

    let _ = app.update(Message::DragEvent(super::dnd::DragEvent::Cancelled));
    assert!(!app.file_browser.dragging);

    let _ = app.update(Message::DragEvent(super::dnd::DragEvent::Started));
    let _ = app.update(Message::DragEvent(super::dnd::DragEvent::Dropped));
    assert!(!app.file_browser.dragging);
}

#[test]
fn test_file_browser_query_change_clears_stale_press() {
    let mut app = browser_app_with_two_dirs();
    app.file_browser.pressed_row = Some(1);

    let _ = app.update(Message::SearchChanged("/tmp/al".into()));

    assert!(app.file_browser.active);
    assert_eq!(app.file_browser.pressed_row, None);
}

#[test]
fn test_file_preview_cache_skips_inspection_when_hidden() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("raffi-hidden-preview-test-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    let first_path = dir.join("first.txt");
    let first_path_str = first_path.to_string_lossy().into_owned();
    std::fs::write(&first_path, "first contents").unwrap();
    let second_path = dir.join("second.txt");
    let second_path_str = second_path.to_string_lossy().into_owned();
    std::fs::write(&second_path, "second contents").unwrap();

    let mut app = test_app(AddonsConfig::default());
    app.keyword_suggestions.clear();
    app.file_browser.active = true;
    app.file_browser
        .entries
        .push(super::state::FileBrowserEntry {
            name: "first.txt".into(),
            full_path: first_path_str.clone(),
            is_dir: false,
        });
    app.file_browser
        .entries
        .push(super::state::FileBrowserEntry {
            name: "second.txt".into(),
            full_path: second_path_str.clone(),
            is_dir: false,
        });

    assert_eq!(app.total_items(), 2);
    app.refresh_file_preview_cache();
    assert_eq!(
        app.file_browser
            .cached_preview
            .as_ref()
            .map(|preview| preview.path.clone()),
        Some(first_path_str)
    );

    let _ = app.update(Message::TogglePreview);
    assert!(!app.view.show_preview);
    assert!(app.file_browser.cached_preview.is_none());

    let _ = app.update(Message::MoveDown);
    assert_eq!(app.selected_index, 1);
    assert!(app.file_browser.cached_preview.is_none());

    let _ = app.update(Message::TogglePreview);
    assert!(app.view.show_preview);
    let cached = app
        .file_browser
        .cached_preview
        .as_ref()
        .expect("preview should be inspected when shown again");
    assert_eq!(cached.path, second_path_str);

    let _ = std::fs::remove_dir_all(dir);
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
fn test_parse_mru_map_keeps_pipes_in_keys() {
    use super::support::parse_mru_map;

    let map = parse_mru_map("sn:a|b|3|100\nFirefox|7|200\nLegacy|2\nbroken|x|1");
    assert_eq!(map["sn:a|b"].count, 3);
    assert_eq!(map["sn:a|b"].last_used, 100);
    assert_eq!(map["Firefox"].count, 7);
    assert_eq!(map["Legacy"].last_used, 0);
    assert_eq!(map.len(), 3);
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
        ..Default::default()
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
        dynamic_preview: None,
        ..Default::default()
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
        ..Default::default()
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
        dynamic_preview: None,
        ..Default::default()
    });
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Snippets".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            icon: None,
            name: "Email".into(),
            value: "user@example.com".into(),
            keyword: None,
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
    });
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
        ..Default::default()
    });
    addons
}

#[test]
fn test_help_entries_cover_every_enabled_addon_in_routing_order() {
    let entries = help_entries(&help_addons());
    let triggers: Vec<&str> = entries.iter().map(|entry| entry.trigger.as_str()).collect();

    assert_eq!(
        triggers,
        vec![
            "~/",
            "gist",
            "sn",
            "emoji",
            "gh",
            "sys",
            "win",
            "tabs",
            "bm",
            "hist",
            "media",
            "audio",
            "kill",
            "bt",
            "pass",
            "bw",
            "dev",
            ">",
            "math",
            "$",
            "units",
            "color",
            "time",
            "uuid/pw",
            "↑/↓",
            "Ctrl+J",
            "Ctrl+K",
            "Ctrl+P",
            "Ctrl+R",
            "Ctrl+H",
            "Ctrl+O",
            "Ctrl+B",
            "Ctrl+Y",
            "Ctrl+Alt+Y",
            "Ctrl+S",
            "Ctrl+D",
            "Ctrl+U",
            "Ctrl+O",
            "Alt+P/N",
            "Tab",
            "Backspace",
            "~/ or / after /",
            "Ctrl+/",
            "Esc",
            "Enter",
            "Ctrl+Enter"
        ]
    );
    let shortcut_start = entries
        .iter()
        .position(|entry| entry.section == HelpSection::KeyboardShortcuts)
        .unwrap();
    assert!(entries[..shortcut_start]
        .iter()
        .all(|entry| entry.section == HelpSection::KeywordsAndActions));
    assert!(entries[shortcut_start..]
        .iter()
        .all(|entry| entry.section == HelpSection::KeyboardShortcuts));
    assert_eq!(entries[1].icon.as_deref(), Some("gist-icon"));
    assert_eq!(entries[1].completion.as_deref(), Some("gist "));
    assert_eq!(entries[0].completion.as_deref(), Some("~/"));
    assert!(entries
        .iter()
        .find(|entry| entry.trigger == "math")
        .is_some_and(|entry| entry.completion.is_none()));
    // Shortcut rows are informational: they never arm a keyword.
    for entry in entries
        .iter()
        .filter(|entry| entry.section == HelpSection::KeyboardShortcuts)
    {
        assert_eq!(entry.completion, None);
    }
}

#[test]
fn test_help_entries_list_keyboard_shortcuts_after_keywords() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.calculator.enabled = false;
    addons.currency.enabled = false;
    addons.file_browser.enabled = false;
    addons.system.enabled = false;
    addons.windows.enabled = false;
    addons.browser_tabs.enabled = false;
    addons.bookmarks.enabled = false;
    addons.browser_history.enabled = false;
    addons.media.enabled = false;
    addons.audio.enabled = false;
    addons.kill.enabled = false;
    addons.bluetooth.enabled = false;
    addons.pass.enabled = false;
    addons.bitwarden.enabled = false;
    addons.units.enabled = false;
    addons.color.enabled = false;
    addons.world_clock.enabled = false;
    addons.devtools.enabled = false;
    addons.terminal.enabled = false;
    let entries = help_entries(&addons);

    // With every addon disabled only the general shortcut rows remain.
    assert!(entries
        .iter()
        .all(|entry| entry.section == HelpSection::KeyboardShortcuts));
    let triggers: Vec<&str> = entries.iter().map(|entry| entry.trigger.as_str()).collect();
    assert_eq!(
        triggers,
        vec![
            "↑/↓",
            "Ctrl+J",
            "Ctrl+K",
            "Ctrl+P",
            "Ctrl+R",
            "Alt+P/N",
            "Tab",
            "Ctrl+/",
            "Esc",
            "Enter",
            "Ctrl+Enter"
        ]
    );

    let by_ctrl: Vec<&str> = filter_help_entries(&entries, "ctrl")
        .into_iter()
        .map(|index| entries[index].trigger.as_str())
        .collect();
    assert_eq!(
        by_ctrl,
        vec![
            "Ctrl+J",
            "Ctrl+K",
            "Ctrl+P",
            "Ctrl+R",
            "Ctrl+/",
            "Ctrl+Enter"
        ]
    );

    let by_scroll: Vec<&str> = filter_help_entries(&entries, "scroll")
        .into_iter()
        .map(|index| entries[index].trigger.as_str())
        .collect();
    assert_eq!(by_scroll, vec!["Ctrl+J", "Ctrl+K"]);
}

#[test]
fn test_help_entries_skip_disabled_addons() {
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.calculator.enabled = false;
    addons.currency.enabled = false;
    addons.file_browser.enabled = false;
    addons.system.enabled = false;
    addons.windows.enabled = false;
    addons.browser_tabs.enabled = false;
    addons.bookmarks.enabled = false;
    addons.browser_history.enabled = false;
    addons.media.enabled = false;
    addons.audio.enabled = false;
    addons.kill.enabled = false;
    addons.bluetooth.enabled = false;
    addons.pass.enabled = false;
    addons.bitwarden.enabled = false;
    addons.units.enabled = false;
    addons.color.enabled = false;
    addons.world_clock.enabled = false;
    addons.devtools.enabled = false;
    addons.terminal.enabled = false;

    // Disabled addons drop their keyword rows and addon-specific shortcuts.
    let entries = help_entries(&addons);
    assert!(entries.iter().all(|entry| entry.completion.is_none()));
    assert!(entries
        .iter()
        .all(|entry| entry.section == HelpSection::KeyboardShortcuts));
    assert_eq!(entries.len(), 11);
}

#[test]
fn test_help_entries_describe_configured_file_actions() {
    let mut addons = AddonsConfig::default();
    addons.file_browser.secondary_action = Some("insert".into());
    let entries = help_entries(&addons);
    let usage = |trigger: &str| {
        entries
            .iter()
            .find(|entry| entry.trigger == trigger)
            .map(|entry| entry.usage.clone())
            .unwrap()
    };

    assert!(usage("Enter").ends_with("File browser files: Open with xdg-open"));
    assert!(usage("Ctrl+Enter").ends_with("File browser: Type full path into focused app"));
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
    assert!(app.search_query.is_empty());
    assert!(app.filtered_configs.is_empty());
    assert!(app.keyword_suggestions.is_empty());
    assert_eq!(app.total_items(), app.help_entries.len());
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
    assert!(app.inline_result.is_none());
}

#[test]
fn test_help_selection_arms_keyword() {
    let mut app = test_app(help_addons());
    app.search_query = "gist ".into();
    let _ = app.update(Message::ToggleHelp);
    assert!(app.search_query.is_empty());
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
    assert!(app.search_query.is_empty());
    let _ = app.update(Message::SearchChanged("Ctrl+P".into()));
    assert_eq!(app.total_items(), 1);
    let _ = app.update(Message::ToggleHelp);
    assert!(!app.view.help_active);
    assert_eq!(app.search_query, "gist ");
    assert!(app.help_filtered.is_empty());
    assert!(app.script_filter.loading || app.script_filter.results.is_some());

    let _ = app.update(Message::ToggleHelp);
    assert!(app.view.help_active);
    assert!(app.search_query.is_empty());
    let _ = app.update(Message::SearchChanged("sn".into()));
    let _ = app.update(Message::Cancel);
    assert!(!app.view.help_active);
    assert_eq!(app.search_query, "gist ");
}

#[test]
fn test_preview_scroll_messages_leave_state_unchanged() {
    let mut app = test_app(AddonsConfig::default());
    super::configure_preview_visibility(&mut app.view, Some(false));
    app.inline_result = Some(super::support::calculator_inline_result(
        &super::types::CalculatorResult {
            expression: "1+1".into(),
            result: 2.0,
        },
    ));
    assert!(!app.view.show_launcher_preview);
    assert!(app.view.show_preview);

    // Scrolling with the pane visible must not change any app state.
    let _ = app.update(Message::PreviewScrollDown);
    let _ = app.update(Message::PreviewScrollUp);
    assert_eq!(app.selected_index, 0);
    assert!(app.view.show_preview);
    assert!(app.resolve_selected_preview().is_some());

    // With the pane hidden the handlers are no-ops, not panics.
    app.view.show_preview = false;
    let _ = app.update(Message::PreviewScrollDown);
    let _ = app.update(Message::PreviewScrollUp);
    assert!(!app.view.show_preview);
    assert_eq!(app.selected_index, 0);
}

#[test]
fn test_window_resize_updates_preview_sizing() {
    let mut app = test_app(AddonsConfig::default());
    assert_eq!(app.view.window_size, iced::Size::new(800.0, 600.0));
    // Default-font chrome is 178px, so a 600px window leaves a 422px viewport.
    assert_eq!(app.preview_viewport_height(), 422.0);
    // Half of the 735px body row, less the pane's 32px of side padding.
    assert_eq!(app.preview_viewport_width(), 335.5);

    // Resizing the window grows the full-pane image preview with it.
    let _ = app.update(Message::WindowResized(iced::Size::new(1200.0, 900.0)));
    assert_eq!(app.view.window_size.width, 1200.0);
    assert_eq!(app.preview_viewport_height(), 722.0);
    assert_eq!(app.preview_viewport_width(), 535.5);

    app.view.preview_style.width_fraction = 0.3;
    // 30% of the 1135px row, less the same 32px.
    assert_eq!(app.preview_viewport_width(), 308.5);

    let _ = app.update(Message::WindowResized(iced::Size::new(800.0, 400.0)));
    assert_eq!(app.preview_viewport_height(), 222.0);
}

#[test]
fn test_toggle_preview_message() {
    let mut app = test_app(AddonsConfig::default());
    assert!(app.view.show_preview);
    assert!(!app.view.show_launcher_preview);

    // Nothing is previewed yet, so the first Ctrl+P opts launcher previews in.
    let _ = app.update(Message::TogglePreview);
    assert!(app.view.show_preview);
    assert!(app.view.show_launcher_preview);

    // Once something would show, Ctrl+P hides the pane entirely.
    app.inline_result = Some(super::support::calculator_inline_result(
        &super::types::CalculatorResult {
            expression: "1+1".into(),
            result: 2.0,
        },
    ));
    assert!(app.resolve_selected_preview().is_some());
    let _ = app.update(Message::TogglePreview);
    assert!(!app.view.show_preview);
    assert!(app.resolve_selected_preview().is_none());

    let _ = app.update(Message::TogglePreview);
    assert!(app.view.show_preview);
    assert!(app.resolve_selected_preview().is_some());
}

#[test]
fn test_launcher_preview_is_opt_in() {
    let configs = vec![
        RaffiConfig {
            binary: Some("firefox".into()),
            description: Some("Firefox".into()),
            ..Default::default()
        },
        RaffiConfig {
            binary: Some("kitty".into()),
            description: Some("Kitty".into()),
            show_preview: Some(true),
            ..Default::default()
        },
    ];
    let (mut app, _) = LauncherApp::new(
        configs,
        AddonsConfig::default(),
        true,
        Arc::new(Mutex::new(None)),
        None,
        ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None),
        10,
        FontSizes::default_sizes(),
        SortMode::Hybrid,
        Vec::new(),
    );
    super::configure_preview_visibility(&mut app.view, Some(false));
    assert!(app.view.show_preview);
    assert!(!app.view.show_launcher_preview);

    // Plain launchers show nothing unless explicitly enabled.
    assert!(app.resolve_selected_preview().is_none());

    // A launcher with `show_preview: true` always previews.
    app.selected_index = 1;
    assert_eq!(
        app.resolve_selected_preview().map(|p| p.title),
        Some("Kitty".to_string())
    );

    // Ctrl+P on a hidden launcher preview turns launcher previews on.
    app.selected_index = 0;
    let _ = app.update(Message::TogglePreview);
    assert!(app.view.show_preview);
    assert!(app.view.show_launcher_preview);
    assert_eq!(
        app.resolve_selected_preview().map(|p| p.title),
        Some("Firefox".to_string())
    );

    // `general.show_preview: true` enables launcher previews globally.
    app.view.show_launcher_preview = false;
    assert!(app.resolve_selected_preview().is_none());
    app.view.show_launcher_preview = true;
    assert!(app.resolve_selected_preview().is_some());
}

#[test]
fn test_preview_style_resolves_defaults_and_overrides() {
    use super::theme::PreviewStyle;
    let theme = ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None);

    let default_style = PreviewStyle::resolve(&theme, &crate::PreviewConfig::default(), None, 20.0);
    assert_eq!(default_style.text, 16.0);
    assert_eq!(default_style.width_fraction, 0.5);
    assert_eq!(default_style.accent, theme.accent);

    let inherited_font_style = PreviewStyle::resolve(
        &theme,
        &crate::PreviewConfig::default(),
        Some("UI Font"),
        26.0,
    );
    assert!(matches!(
        inherited_font_style.font.family,
        iced::font::Family::Name("UI Font")
    ));
    assert_eq!(inherited_font_style.text, 21.0);

    let config = crate::PreviewConfig {
        font_size: Some(20.0),
        width_percent: Some(95.0),
        font_family: Some("Preview Font".into()),
        colors: Some(crate::PreviewColorsConfig {
            accent: Some("#ff0000".into()),
            bg: Some("#00000080".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let style = PreviewStyle::resolve(&theme, &config, Some("UI Font"), 26.0);
    assert!(matches!(
        style.font.family,
        iced::font::Family::Name("Preview Font")
    ));
    assert_eq!(style.text, 20.0);
    assert_eq!(style.title, 28.0);
    assert_eq!(style.width_fraction, 0.8); // clamped
    assert_eq!(style.accent, iced::Color::from_rgb8(255, 0, 0));
    assert!((style.bg.a - 128.0 / 255.0).abs() < 0.01);
    assert_eq!(style.text_main, theme.text_main);
}

#[test]
fn test_theme_colors_presets_and_overrides() {
    let presets = [
        crate::ThemeMode::Dark,
        crate::ThemeMode::Light,
        crate::ThemeMode::CatppuccinMocha,
        crate::ThemeMode::CatppuccinLatte,
        crate::ThemeMode::TokyoNight,
        crate::ThemeMode::Nord,
        crate::ThemeMode::Gruvbox,
        crate::ThemeMode::Dracula,
        crate::ThemeMode::RosePine,
        crate::ThemeMode::Kanagawa,
    ];
    for mode in &presets {
        let colors = ThemeColors::from_mode_with_overrides(mode, None);
        assert_ne!(colors.text_main, colors.bg_base);
        assert_ne!(colors.text_muted, colors.bg_base);
        // RAFFI_THEME must name the preset the way the config does.
        assert_eq!(
            colors.name.parse::<crate::ThemeMode>().ok().as_ref(),
            Some(mode)
        );
        assert_eq!(colors.light, mode.is_light());
    }

    let overrides = crate::ThemeColorsConfig {
        accent: Some("#112233".into()),
        ..Default::default()
    };
    let customized =
        ThemeColors::from_mode_with_overrides(&crate::ThemeMode::TokyoNight, Some(&overrides));
    assert_eq!(customized.accent, iced::Color::from_rgb8(0x11, 0x22, 0x33));
    assert_eq!(
        customized.bg_input,
        ThemeColors::from_mode_with_overrides(&crate::ThemeMode::TokyoNight, None).bg_input
    );
    let env: std::collections::HashMap<_, _> = customized.command_env().into_iter().collect();
    assert_eq!(env["RAFFI_THEME"], "tokyo-night");
    assert_eq!(env["RAFFI_THEME_MODE"], "dark");
    assert_eq!(env["RAFFI_THEME_ACCENT"], "#112233");
}

#[test]
fn test_theme_opacity_scales_only_backgrounds() {
    let base = ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Nord, None);
    let half = base.with_opacity(0.5);
    for (scaled, orig) in [
        (half.bg_base, base.bg_base),
        (half.bg_input, base.bg_input),
        (half.selection_bg, base.selection_bg),
    ] {
        assert!((scaled.a - orig.a * 0.5).abs() < f32::EPSILON);
        assert_eq!((scaled.r, scaled.g, scaled.b), (orig.r, orig.g, orig.b));
    }
    assert_eq!(half.text_main, base.text_main);
    assert_eq!(half.accent, base.accent);
    assert_eq!(half.border, base.border);
    assert_eq!(base.with_opacity(7.0).bg_base, base.bg_base);
    assert_eq!(base.with_opacity(f32::NAN).bg_base, base.bg_base);
}

#[test]
fn test_preview_ansi_palette_swaps_unreadable_colours() {
    use super::theme::PreviewStyle;
    let config = crate::PreviewConfig::default();
    // Palette black sits on a dark background: unreadable, so muted.
    let dark = ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None);
    let style = PreviewStyle::resolve(&dark, &config, None, 20.0);
    assert_eq!(style.ansi[0], style.text_muted);
    assert_eq!(style.ansi[1], dark.ansi[1]);
    // Light palettes put a pale colour in slot 0 and a dark one in slot 7.
    let latte = ThemeColors::from_mode_with_overrides(&crate::ThemeMode::CatppuccinLatte, None);
    let style = PreviewStyle::resolve(&latte, &config, None, 20.0);
    assert_eq!(style.ansi[0], style.text_muted);
    assert_eq!(style.ansi[7], latte.ansi[7]);
}

#[test]
fn test_preview_launcher_binary_and_script() {
    let configs = vec![
        RaffiConfig {
            binary: Some("jumpapp".into()),
            args: Some(vec!["-X".into(), "firefox".into()]),
            description: Some("Firefox".into()),
            ifexist: Some("firefox".into()),
            ..Default::default()
        },
        RaffiConfig {
            script: Some("echo hello\necho world".into()),
            description: Some("Greeting Script".into()),
            ..Default::default()
        },
    ];
    let (mut app, _) = LauncherApp::new(
        configs,
        AddonsConfig::default(),
        true,
        Arc::new(Mutex::new(None)),
        None,
        ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None),
        10,
        FontSizes::default_sizes(),
        SortMode::Hybrid,
        Vec::new(),
    );
    app.view.show_launcher_preview = true;

    let preview = app
        .resolve_selected_preview()
        .expect("expected launcher preview");
    assert_eq!(preview.title, "Firefox");
    assert_eq!(preview.badge.as_deref(), Some("Launcher"));
    assert!(preview.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Text { content, .. } if content == "jumpapp -X firefox"
    )));
    assert!(preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Condition" && v == "ifexist: firefox"));

    app.selected_index = 1;
    let script_preview = app
        .resolve_selected_preview()
        .expect("expected script preview");
    assert_eq!(script_preview.title, "Greeting Script");
    assert_eq!(script_preview.badge.as_deref(), Some("Script"));
    assert!(script_preview.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Text { content, .. } if content == "echo hello\necho world"
    )));
}

#[test]
fn test_preview_text_snippet_shows_full_multiline_and_raw_template() {
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Templates".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(vec![TextSnippet {
            name: "Bug Report".into(),
            keyword: Some("!bug".into()),
            icon: Some("🐛".into()),
            value: "Date: {date:%Y}\nSteps:\n1. {cursor}\n2. Expected".into(),
        }]),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: Some("copy".into()),
        secondary_action: Some("insert".into()),
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
    });

    let mut app = test_app(addons);
    let _ = app.update(Message::SearchChanged("sn ".into()));

    let preview = app
        .resolve_selected_preview()
        .expect("expected snippet preview");
    assert_eq!(preview.title, "Bug Report");
    assert_eq!(preview.badge.as_deref(), Some("!bug"));
    // Both expanded content and raw template should be present since {date:%Y} expands
    assert_eq!(preview.bodies.len(), 2);
    assert!(preview.metadata.iter().any(|(k, _)| k == "Cursor"));
}

#[test]
fn test_preview_script_filter_precedence() {
    let mut app = test_app(AddonsConfig::default());
    app.script_filter.results = Some(ScriptFilterResult {
        default_icon: None,
        items: vec![
            ScriptFilterItem {
                title: "Item 1".into(),
                valid: true,
                subtitle: Some("Fallback subtitle".into()),
                arg: Some("arg1".into()),
                indent: 0,
                icon: None,
                preview: Some(super::types::ScriptFilterPreview::Text(
                    "Explicit preview body".into(),
                )),
                quicklookurl: None,
                variables: Default::default(),
                text: Some(ScriptFilterItemText {
                    copy: Some("copy1".into()),
                    largetype: Some("Large type body".into()),
                }),
            },
            ScriptFilterItem {
                title: "Item 2".into(),
                valid: true,
                subtitle: Some("Fallback subtitle".into()),
                arg: Some("arg2".into()),
                indent: 0,
                icon: None,
                preview: None,
                quicklookurl: None,
                variables: Default::default(),
                text: Some(ScriptFilterItemText {
                    copy: None,
                    largetype: Some("Large type body".into()),
                }),
            },
        ],
    });

    let p1 = app
        .resolve_selected_preview()
        .expect("expected script filter preview 1");
    assert!(p1.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::AnsiText { content, .. } if content == "Explicit preview body"
    )));

    app.selected_index = 1;
    let p2 = app
        .resolve_selected_preview()
        .expect("expected script filter preview 2");
    assert!(p2.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::AnsiText { content, .. } if content == "Large type body"
    )));
}

#[test]
fn test_preview_script_filter_decodes_file_url_paths() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("raffi-preview-url-test-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("report one.txt");
    std::fs::write(&file_path, "file contents").unwrap();
    let escaped_path = file_path.to_string_lossy().replace(' ', "%20");
    let file_url = format!("file://{escaped_path}");
    let localhost_file_url = format!("file://localhost{escaped_path}");
    let make_item = |quicklookurl| ScriptFilterItem {
        title: "File".into(),
        valid: true,
        subtitle: None,
        arg: None,
        indent: 0,
        icon: None,
        preview: None,
        quicklookurl: Some(quicklookurl),
        variables: Default::default(),
        text: None,
    };

    let mut app = test_app(AddonsConfig::default());
    app.keyword_suggestions.clear();
    app.script_filter.results = Some(ScriptFilterResult {
        default_icon: None,
        items: vec![make_item(file_url), make_item(localhost_file_url)],
    });

    for selected_index in 0..2 {
        app.selected_index = selected_index;
        let preview = app
            .resolve_selected_preview()
            .expect("expected Script Filter preview");
        assert!(preview.bodies.iter().any(|body| matches!(
            body,
            PreviewBody::Text {
                heading: Some(heading),
                content,
                ..
            } if heading == "Contents" && content == "file contents"
        )));
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

fn dynamic_preview_app(items: Vec<ScriptFilterItem>) -> LauncherApp {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "Agents".into(),
        command: "agents".into(),
        keyword: "ag".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
        dynamic_preview: None,
        ..Default::default()
    });
    let mut app = test_app(addons);
    app.keyword_suggestions.clear();
    app.view.show_preview = true;
    app.script_filter.config_index = Some(0);
    app.script_filter.results = Some(ScriptFilterResult {
        default_icon: None,
        items,
    });
    app
}

fn plain_item(title: &str) -> ScriptFilterItem {
    ScriptFilterItem {
        title: title.into(),
        valid: true,
        subtitle: None,
        arg: Some(format!("{title}-arg")),
        indent: 0,
        icon: None,
        preview: Some(super::types::ScriptFilterPreview::Text(
            "static body".into(),
        )),
        quicklookurl: None,
        variables: Default::default(),
        text: None,
    }
}

#[test]
fn test_script_filter_invalid_headings_do_not_activate_or_record() {
    let mut heading = plain_item("Working");
    heading.valid = false;
    let mut app = dynamic_preview_app(vec![heading]);
    app.main_view.names = vec![Some("script_filters.Agents".into())];
    app.search_query = "ag working".into();
    app.script_filter.action = Some("true".into());
    app.script_filter.secondary_action = Some("true".into());
    app.history.items.clear();
    app.main_view.recent.clear();
    for modifiers in [
        iced::keyboard::Modifiers::empty(),
        iced::keyboard::Modifiers::CTRL,
    ] {
        app.view.current_modifiers = modifiers;
        let task = app.handle_script_filter_selected(0);
        assert_eq!(task.units(), 0, "no exit or action task");
        assert!(app.main_view.recent.is_empty());
        assert!(app.history.items.is_empty());
        assert_eq!(app.search_query, "ag working");
        let preview = app.resolve_selected_preview().unwrap();
        assert!(!preview
            .metadata
            .iter()
            .any(|(name, _)| name == "Enter" || name == "Ctrl+Enter"));
    }
}

#[test]
fn test_script_filter_invalid_main_view_heading_preserves_mode() {
    use super::main_view::{Block, Source};
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(issues_filter());
    let mut app = main_view_app(&["script_filters.Issues"], Vec::new(), addons);
    let _ = app.update(Message::SearchChanged("working".into()));
    let mut heading = plain_item("Working");
    heading.valid = false;
    let Source::ScriptFilter { results, .. } = &mut app.main_view.sources[0] else {
        panic!("script filter source");
    };
    *results = Some(ScriptFilterResult {
        default_icon: None,
        items: vec![heading],
    });
    app.main_view.recent.clear();
    app.history.items.clear();
    for modifiers in [
        iced::keyboard::Modifiers::empty(),
        iced::keyboard::Modifiers::CTRL,
    ] {
        app.view.current_modifiers = modifiers;
        let task = app.activate_main_view(Block::BeforeLaunchers, 0);
        assert_eq!(task.units(), 0);
        assert_eq!(app.search_query, "working");
        assert!(app.script_filter.config_index.is_none());
        assert!(app.main_view.recent.is_empty());
        assert!(app.history.items.is_empty());
    }
}

#[test]
fn test_select_1_ignores_script_filter_invalid_headings() {
    let mut heading = plain_item("Working");
    heading.valid = false;
    let mut app = dynamic_preview_app(vec![heading.clone()]);
    app.main_view.names = vec![Some("script_filters.Agents".into())];
    app.search_query = "ag working".into();
    app.script_filter.action = Some("true".into());
    app.select_1_query = Some(app.search_query.clone());
    app.select_1_ready = true;
    app.view.show_preview = false;
    assert_eq!(app.update(Message::Submit).units(), 0);
    assert!(app.select_1_query.is_none());
    assert!(app.main_view.recent.is_empty());

    app.script_filter
        .results
        .as_mut()
        .unwrap()
        .items
        .push(plain_item("Agent"));
    app.select_1_query = Some(app.search_query.clone());
    app.select_1_ready = true;
    let generation = app.script_filter.preview_generation;
    assert!(
        app.update(Message::ScriptFilterPreviewDue(
            generation.wrapping_sub(1),
            0
        ))
        .units()
            > 0
    );
    assert_eq!(app.main_view.recent.len(), 1);
    assert_eq!(app.main_view.recent[0].key, "Agent-arg");
}

fn prepared(text: &str, refresh: Option<std::time::Duration>) -> super::types::PreparedPreview {
    super::types::PreparedPreview {
        title: Some("Dynamic title".into()),
        bodies: vec![super::types::PreparedPreviewBody::Text {
            heading: None,
            content: text.into(),
        }],
        replaces_body: true,
        metadata: vec![("Status".into(), "working".into())],
        refresh,
        dynamic: true,
        ..Default::default()
    }
}

#[test]
fn test_dynamic_preview_replaces_static_preview_and_reports_errors() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);

    let generation = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(prepared("live output", None))),
    ));
    let preview = app.resolve_selected_preview().expect("preview");
    assert_eq!(preview.title, "Dynamic title");
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { content, .. }] if content == "live output"
    ));
    assert_eq!(
        preview.metadata.first(),
        Some(&("Status".to_string(), "working".to_string()))
    );
    assert!(preview.metadata.iter().any(|(k, _)| k == "Enter"));

    // A later failure keeps the last good preview and says why.
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Err(super::types::ScriptFilterPreviewError::Run(
            "timed out after 3s".into(),
        )),
    ));
    let preview = app.resolve_selected_preview().expect("preview");
    assert_eq!(preview.title, "Dynamic title");
    assert!(preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Preview" && v == "timed out after 3s"));
}

#[test]
fn test_dynamic_preview_ignores_stale_generations() {
    let mut app = dynamic_preview_app(vec![plain_item("one"), plain_item("two")]);
    let stale = app.script_filter.preview_generation;
    let _ = app.update(Message::MoveDown);
    assert_ne!(app.script_filter.preview_generation, stale);

    let _ = app.update(Message::ScriptFilterPreview(
        stale,
        0,
        0,
        Ok(Some(prepared("old", None))),
    ));
    assert!(app.script_filter.previews.is_empty());
}

#[test]
fn test_dynamic_preview_disabled_for_scripts_without_preview_support() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let _ = app.request_script_filter_preview();
    let generation = app.script_filter.preview_generation;
    assert_eq!(app.dynamic_preview_target(), Some(0));

    let _ = app.update(Message::ScriptFilterPreview(generation, 0, 0, Ok(None)));
    assert!(app.script_filter.preview_unsupported.contains(&0));
    assert_eq!(app.dynamic_preview_target(), None);

    // The static preview is still shown.
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { content, .. }] if content == "static body"
    ));

    // Leaving script filter mode does not forget it.
    app.script_filter.clear();
    assert!(app.script_filter.preview_unsupported.contains(&0));
}

#[test]
fn test_dynamic_preview_opt_outs() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    app.addons.script_filters[0].dynamic_preview = Some(false);
    assert_eq!(app.dynamic_preview_target(), None);

    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    app.view.show_preview = false;
    assert_eq!(app.dynamic_preview_target(), None);

    let mut item = plain_item("one");
    item.preview = Some(super::types::ScriptFilterPreview::Rich(Box::new(
        super::types::PreparedPreview {
            dynamic: false,
            ..prepared("static rich", None)
        },
    )));
    let app = dynamic_preview_app(vec![item]);
    assert_eq!(app.dynamic_preview_target(), None);
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { content, .. }] if content == "static rich"
    ));
}

#[test]
fn test_dynamic_preview_cache_skips_rerun_unless_refreshing() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let generation = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(prepared("cached", None))),
    ));
    // Moving onto a cached item without `refresh` must not start a new run;
    // the generation still moves so pending work is dropped.
    let before = app.script_filter.preview_generation;
    let _ = app.request_script_filter_preview();
    assert_ne!(app.script_filter.preview_generation, before);
    assert!(app.script_filter.previews.contains_key(&0));
}

#[test]
fn test_dynamic_preview_first_run_failure_disables_it_without_an_error_row() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let _ = app.request_script_filter_preview();
    let generation = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreviewDue(generation, 0));
    assert!(app.script_filter.preview_probing.contains(&0));

    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Err(super::types::ScriptFilterPreviewError::Run(
            "timed out after 3s".into(),
        )),
    ));
    assert!(app.script_filter.preview_unsupported.contains(&0));
    assert!(app.script_filter.previews.is_empty());
    assert_eq!(app.dynamic_preview_target(), None);
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(!preview.metadata.iter().any(|(k, _)| k == "Preview"));
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { content, .. }] if content == "static body"
    ));
}

#[test]
fn test_dynamic_preview_unreadable_first_answer_shows_the_error() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let _ = app.request_script_filter_preview();
    let generation = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreviewDue(generation, 0));

    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Err(super::types::ScriptFilterPreviewError::Invalid(
            "Invalid preview: bad title".into(),
        )),
    ));
    assert!(app.script_filter.preview_supported.contains(&0));
    assert!(!app.script_filter.preview_unsupported.contains(&0));
    assert_eq!(app.dynamic_preview_target(), Some(0));
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Preview" && v == "Invalid preview: bad title"));
}

#[test]
fn test_dynamic_preview_later_non_preview_output_shows_an_error() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let generation = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(prepared("live output", None))),
    ));
    assert!(app.script_filter.preview_supported.contains(&0));

    // Once the script has printed a preview object, plain output is an error.
    let _ = app.update(Message::ScriptFilterPreview(generation, 0, 0, Ok(None)));
    assert!(!app.script_filter.preview_unsupported.contains(&0));
    assert_eq!(app.dynamic_preview_target(), Some(0));
    let preview = app.resolve_selected_preview().expect("preview");
    assert_eq!(preview.title, "Dynamic title");
    assert!(preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Preview" && v == "Not a preview object"));
}

#[test]
fn test_dynamic_preview_probes_legacy_scripts_once() {
    let mut app = dynamic_preview_app(vec![plain_item("one"), plain_item("two")]);
    let _ = app.request_script_filter_preview();
    let probe = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreviewDue(probe, 0));
    assert!(app.script_filter.preview_probing.contains(&0));

    // While the first run is out, moving on does not start another one.
    let _ = app.update(Message::MoveDown);
    let current = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreviewDue(current, 1));
    assert!(app.script_filter.preview_probing.contains(&0));

    // Its stale reply still tells Raffi the script does not do previews.
    let _ = app.update(Message::ScriptFilterPreview(probe, 0, 0, Ok(None)));
    assert!(app.script_filter.preview_probing.is_empty());
    assert!(app.script_filter.preview_unsupported.contains(&0));
    assert_eq!(app.dynamic_preview_target(), None);
}

#[test]
fn test_dynamic_preview_stale_probe_success_marks_script_supported() {
    let mut app = dynamic_preview_app(vec![plain_item("one"), plain_item("two")]);
    let _ = app.request_script_filter_preview();
    let probe = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreviewDue(probe, 0));
    let _ = app.update(Message::MoveDown);

    let _ = app.update(Message::ScriptFilterPreview(
        probe,
        0,
        0,
        Ok(Some(prepared("old", None))),
    ));
    assert!(app.script_filter.preview_supported.contains(&0));
    assert!(app.script_filter.previews.is_empty());

    // Once supported, runs are no longer serialised behind a probe.
    let current = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreviewDue(current, 1));
    assert!(app.script_filter.preview_probing.is_empty());
}

#[test]
fn test_dynamic_preview_empty_sections_clear_static_body() {
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let generation = app.script_filter.preview_generation;
    let mut cleared = prepared("unused", None);
    cleared.bodies.clear();
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(cleared)),
    ));
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(preview.bodies.is_empty());

    // A metadata-only answer keeps the static body.
    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let generation = app.script_filter.preview_generation;
    let metadata_only = super::types::PreparedPreview {
        replaces_body: false,
        bodies: Vec::new(),
        ..prepared("unused", None)
    };
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(metadata_only)),
    ));
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { content, .. }] if content == "static body"
    ));
}

#[test]
fn test_dynamic_preview_file_section_rereads_same_path() {
    let dir = std::env::temp_dir().join(format!("raffi-sf-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("status.txt");
    std::fs::write(&file, "first version").unwrap();
    let path = file.to_string_lossy().to_string();
    let file_preview = || super::types::PreparedPreview {
        bodies: vec![super::types::PreparedPreviewBody::File(path.clone())],
        ..prepared("unused", Some(std::time::Duration::from_secs(2)))
    };

    let mut app = dynamic_preview_app(vec![plain_item("one")]);
    let generation = app.script_filter.preview_generation;
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(file_preview())),
    ));
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(format!("{:?}", preview.bodies).contains("first version"));

    std::fs::write(&file, "second version").unwrap();
    let _ = app.update(Message::ScriptFilterPreview(
        generation,
        0,
        0,
        Ok(Some(file_preview())),
    ));
    let preview = app.resolve_selected_preview().expect("preview");
    assert!(format!("{:?}", preview.bodies).contains("second version"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_preview_emoji_calculator_currency_and_web_search() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://github.com/search?q={query}".into(),
        icon: None,
        ..Default::default()
    });

    let mut app = test_app(addons);

    // Calculator
    let _ = app.update(Message::SearchChanged("16 * 16".into()));
    let calc_preview = app
        .resolve_selected_preview()
        .expect("expected calculator preview");
    assert_eq!(calc_preview.title, "Calculator");
    assert!(calc_preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Hexadecimal" && v == "0x100"));
    assert!(calc_preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Binary" && v == "0b100000000"));

    // Web search
    let _ = app.update(Message::SearchChanged("gh rust iced".into()));
    let ws_preview = app
        .resolve_selected_preview()
        .expect("expected web search preview");
    assert_eq!(ws_preview.title, "GitHub");
    assert!(ws_preview.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Text { content, .. } if content == "https://github.com/search?q=rust%20iced"
    )));

    // Emoji
    app.web_search.clear();
    app.emoji.active = true;
    app.emoji.data = vec![EmojiEntry {
        name: "grinning face".into(),
        value: "😀".into(),
    }];
    app.emoji.filtered = vec![0];
    app.selected_index = 0;
    let emoji_preview = app
        .resolve_selected_preview()
        .expect("expected emoji preview");
    assert_eq!(emoji_preview.title, "grinning face");
    assert!(emoji_preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Codepoints" && v == "U+1F600"));

    // Currency
    app.emoji.clear();
    app.currency.result = Some(CurrencyResult {
        request: CurrencyConversionRequest {
            amount: 100.0,
            from_currency: "USD".into(),
            to_currency: "EUR".into(),
        },
        converted_amount: 92.5,
        rate: 0.925,
    });
    let curr_preview = app
        .resolve_selected_preview()
        .expect("expected currency preview");
    assert_eq!(curr_preview.title, "USD → EUR");
    assert!(curr_preview
        .metadata
        .iter()
        .any(|(k, v)| k == "Rate" && v == "1 USD = 0.9250 EUR"));
}

#[test]
fn test_inspect_path_text_binary_truncation_and_directory() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("raffi-preview-test-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();

    // Empty regular file
    let empty_path = dir.join("empty.txt");
    std::fs::write(&empty_path, "").unwrap();
    let empty_inspected = inspect_path(empty_path.to_str().unwrap(), false);
    assert!(empty_inspected
        .metadata
        .iter()
        .any(|(k, v)| k == "Type" && v == "Empty file"));
    assert!(empty_inspected.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Prose { content, .. } if content == "(Empty file)"
    )));

    // Text file
    let txt_path = dir.join("hello.txt");
    std::fs::write(&txt_path, "line 1\nline 2\n").unwrap();
    let txt_inspected = inspect_path(txt_path.to_str().unwrap(), false);
    assert!(txt_inspected.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Text { content, truncated: false, .. } if content == "line 1\nline 2"
    )));

    // Binary file with NUL byte
    let bin_path = dir.join("data.bin");
    std::fs::write(&bin_path, b"hello\0world").unwrap();
    let bin_inspected = inspect_path(bin_path.to_str().unwrap(), false);
    assert!(bin_inspected
        .metadata
        .iter()
        .any(|(k, v)| k == "Type" && v == "Binary file"));

    // Large file exceeding MAX_FILE_PREVIEW_BYTES
    let big_path = dir.join("big.txt");
    let big_content = "a".repeat(MAX_FILE_PREVIEW_BYTES + 512);
    std::fs::write(&big_path, &big_content).unwrap();
    let big_inspected = inspect_path(big_path.to_str().unwrap(), false);
    assert!(big_inspected.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Text { content, truncated: true, .. } if content.len() == MAX_FILE_PREVIEW_BYTES
    )));

    // Many lines exceeding MAX_FILE_PREVIEW_LINES
    let lines_path = dir.join("lines.txt");
    let many_lines = (0..150)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&lines_path, &many_lines).unwrap();
    let lines_inspected = inspect_path(lines_path.to_str().unwrap(), false);
    assert!(lines_inspected.bodies.iter().any(|b| matches!(
        b,
        PreviewBody::Text { content, truncated: true, .. } if content.lines().count() == MAX_FILE_PREVIEW_LINES
    )));

    // Directory inspection
    let dir_inspected = inspect_path(dir.to_str().unwrap(), false);
    assert!(dir_inspected
        .bodies
        .iter()
        .any(|b| matches!(b, PreviewBody::DirectoryList { total_count: 5, .. })));

    assert_eq!(format_byte_size(500), "500 B");
    assert_eq!(format_byte_size(2048), "2.0 KiB");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_fido_updir_goes_to_parent_directory() {
    let home = "/home/user";
    assert_eq!(
        fido_updir("/usr/share/doc/", "/usr/share/doc", home),
        Some(("/usr/share/".into(), "doc".into()))
    );
    assert_eq!(
        fido_updir("/usr/", "/usr", home),
        Some(("/".into(), "usr".into()))
    );
    assert_eq!(
        fido_updir("~/Documents/", "~/Documents", home),
        Some(("~/".into(), "Documents".into()))
    );
    assert_eq!(
        fido_updir("~/", "~", home),
        Some(("/home/".into(), "user".into()))
    );
}

#[test]
fn test_fido_updir_leaves_other_edits_alone() {
    let home = "/home/user";
    // Deleting the root slash is a plain delete.
    assert_eq!(fido_updir("/", "", home), None);
    // Deleting a filter character, not the trailing slash.
    assert_eq!(fido_updir("/usr/sh", "/usr/s", home), None);
    // Typing rather than deleting.
    assert_eq!(fido_updir("/usr", "/usr/", home), None);
    // A doubled slash collapses normally.
    assert_eq!(fido_updir("/usr//", "/usr/", home), None);
}

fn updir_test_dir() -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("raffi-updir-test-{unique}"));
    for name in ["alpha", "beta", "gamma"] {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
    dir
}

#[test]
fn test_backspace_on_trailing_slash_goes_up_and_selects_previous_dir() {
    let dir = updir_test_dir();
    let base = dir.to_string_lossy().into_owned();
    let mut app = test_app(AddonsConfig::default());

    let _ = app.update(Message::SearchChanged(format!("{base}/gamma/")));
    assert!(app.file_browser.active);
    let _ = app.update(Message::SearchChanged(format!("{base}/gamma")));

    assert_eq!(app.search_query, format!("{base}/"));
    let selected = &app.file_browser.entries[app.selected_index];
    assert_eq!(selected.name, "gamma");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_tilde_path_shortens_home() {
    let home = "/home/user/";
    assert_eq!(tilde_path("/home/user/Documents", home), "~/Documents");
    assert_eq!(tilde_path("/home/user", home), "~");
    assert_eq!(tilde_path("/home/username/x", home), "/home/username/x");
    assert_eq!(tilde_path("/etc", ""), "/etc");
}

#[test]
fn test_fido_restart_on_home_or_root_after_a_slash() {
    assert_eq!(
        fido_restart("~/Documents/~", "~/Documents/~/"),
        Some("~/".into())
    );
    assert_eq!(
        fido_restart("/usr/share/", "/usr/share//"),
        Some("/".into())
    );
    // A name ending in ~, a backup file for instance, is a plain directory.
    assert_eq!(fido_restart("~/notes~", "~/notes~/"), None);
    // Only typing at the end restarts.
    assert_eq!(fido_restart("~/a/~/b", "~/a/~//b"), None);
}

#[test]
fn test_typing_home_after_a_path_restarts_the_file_browser() {
    let dir = updir_test_dir();
    let base = dir.to_string_lossy().into_owned();
    let mut app = test_app(AddonsConfig::default());

    let _ = app.update(Message::SearchChanged(format!("{base}/gamma/~")));
    let _ = app.update(Message::SearchChanged(format!("{base}/gamma/~/")));
    assert_eq!(app.search_query, "~/");
    assert!(app.file_browser.active);

    let _ = app.update(Message::SearchChanged("~//".into()));
    assert_eq!(app.search_query, "/");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_fido_style_disabled_is_a_plain_delete() {
    let dir = updir_test_dir();
    let base = dir.to_string_lossy().into_owned();
    let mut addons = AddonsConfig::default();
    addons.file_browser.fido_style = false;
    let mut app = test_app(addons);

    let _ = app.update(Message::SearchChanged(format!("{base}/gamma/")));
    let _ = app.update(Message::SearchChanged(format!("{base}/gamma")));

    assert_eq!(app.search_query, format!("{base}/gamma"));
    assert_eq!(app.selected_index, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

fn clipboard_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = Some(true);
    addons
}

fn clip(row: usize, id: &str, text: &str) -> super::clipboard::ClipItem {
    super::clipboard::ClipItem {
        row,
        id: id.into(),
        formats: vec!["text/plain".into()],
        kind: super::clipboard::ItemKind::Text,
        text_len: text.len(),
        excerpt: text.into(),
        tier: 0,
        copied: None,
    }
}

fn clip_page(
    head: &str,
    total: usize,
    next: usize,
    items: Vec<super::clipboard::ClipItem>,
) -> super::clipboard::Page {
    super::clipboard::Page {
        tab: "&clipboard".into(),
        total,
        head: head.into(),
        next,
        stable: true,
        items,
    }
}

fn clip_text_preview(text: &str) -> super::clipboard::ClipPreview {
    super::clipboard::ClipPreview {
        kind: super::clipboard::ItemKind::Text,
        formats: vec![("text/plain".into(), text.len())],
        text: text.into(),
        text_truncated: false,
        image: None,
        files: Vec::new(),
    }
}

fn open_clipboard(app: &mut LauncherApp, query: &str) -> u64 {
    let _ = app.update(Message::SearchChanged(query.into()));
    assert!(app.clipboard.active);
    app.clipboard.generation
}

#[test]
fn test_clipboard_routing_and_suggestion() {
    assert_eq!(
        route_query("clip ", &AddonsConfig::default()),
        QueryMode::Standard
    );
    let mut addons = clipboard_addons();
    assert_eq!(
        route_query("clip  two words ", &addons),
        QueryMode::Clipboard {
            query: "two words".into(),
        }
    );
    assert_eq!(route_query("clip", &addons), QueryMode::Standard);
    let suggestions = keyword_suggestions("cli", &addons);
    let keywords: Vec<_> = suggestions.iter().map(|s| s.keyword.as_str()).collect();
    assert_eq!(keywords, vec!["clip"]);
    assert_eq!(suggestions[0].icon.as_deref(), Some("copyq"));

    addons.clipboard.trigger = Some("cb".into());
    assert_eq!(
        route_query("cb x", &addons),
        QueryMode::Clipboard { query: "x".into() }
    );
    assert_eq!(route_query("clip x", &addons), QueryMode::Standard);
    assert!(help_entries(&addons)
        .iter()
        .any(|entry| entry.title == "Clipboard history" && entry.trigger == "cb"));
}

#[test]
fn test_clipboard_queries_stay_out_of_history_and_logs() {
    let addons = clipboard_addons();
    assert_eq!(
        loggable_query("clip my password", &addons),
        "clip <redacted>"
    );
    assert_eq!(loggable_query(" firefox ", &addons), "firefox");

    let mut app = test_app(addons);
    let before = app.history.items.clone();
    open_clipboard(&mut app, "clip my password");
    app.save_query_to_history();
    assert_eq!(app.history.items, before);
}

#[test]
fn test_clipboard_scan_pages_merge_and_ignore_stale_generations() {
    let mut app = test_app(clipboard_addons());
    let first = open_clipboard(&mut app, "clip ");
    assert!(app.clipboard.loading);
    assert_eq!(app.total_items(), 1, "status row while loading");
    assert_eq!(app.clipboard.status(), "Loading clipboard history...");

    let _ = app.update(Message::ClipboardPage(
        first.wrapping_sub(1),
        Ok(clip_page("h", 3, 3, vec![clip(0, "stale", "stale")])),
    ));
    assert!(app.clipboard.items.is_empty());

    let _ = app.update(Message::ClipboardPage(
        first,
        Ok(clip_page(
            "h",
            3,
            2,
            vec![clip(0, "a", "alpha"), clip(1, "b", "beta")],
        )),
    ));
    assert_eq!(app.clipboard.items.len(), 2);
    assert!(app.clipboard.loading, "more rows to scan");
    assert_eq!(app.clipboard.scanned, 2);

    // A duplicate of an earlier item is listed once, at its newest row.
    let _ = app.update(Message::ClipboardPage(
        first,
        Ok(clip_page("h", 3, 3, vec![clip(2, "a", "alpha")])),
    ));
    let rows: Vec<_> = app.clipboard.items.iter().map(|i| i.row).collect();
    assert_eq!(rows, vec![0, 1]);
    assert!(!app.clipboard.loading);
    assert_eq!(app.total_items(), 2);

    // Typing a new query starts a new scan; pages of the old one are dropped.
    let second = open_clipboard(&mut app, "clip be");
    assert_ne!(first, second);
    let _ = app.update(Message::ClipboardPage(
        first,
        Ok(clip_page("h", 3, 3, vec![clip(0, "x", "x")])),
    ));
    assert_eq!(
        app.clipboard.items.len(),
        2,
        "old items kept until new page"
    );
    let mut beta = clip(1, "b", "beta");
    beta.tier = 0;
    let mut alpha = clip(0, "a", "alphabet");
    alpha.tier = 1;
    let _ = app.update(Message::ClipboardPage(
        second,
        Ok(clip_page("h", 3, 3, vec![alpha, beta])),
    ));
    let ids: Vec<_> = app.clipboard.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, vec!["b", "a"], "better match first");

    let _ = app.update(Message::SearchChanged("fire".into()));
    assert!(!app.clipboard.active);
    assert!(app.clipboard.items.is_empty());
}

#[test]
fn test_clipboard_scan_restarts_when_history_changes() {
    let mut app = test_app(clipboard_addons());
    let generation = open_clipboard(&mut app, "clip ");
    let _ = app.update(Message::ClipboardPage(
        generation,
        Ok(clip_page(
            "h1",
            4,
            2,
            vec![clip(0, "a", "a"), clip(1, "b", "b")],
        )),
    ));
    // The newest item changed between pages: the scan starts over and the
    // listed items stay visible meanwhile.
    let _ = app.update(Message::ClipboardPage(
        generation,
        Ok(clip_page("h2", 5, 4, vec![clip(2, "c", "c")])),
    ));
    assert_ne!(app.clipboard.generation, generation);
    assert!(app.clipboard.fresh);
    assert_eq!(app.clipboard.items.len(), 2);

    // A history that never settles ends in an error instead of looping.
    for _ in 0..3 {
        let mut page = clip_page("h", 1, 1, vec![clip(0, "z", "z")]);
        page.stable = false;
        let _ = app.update(Message::ClipboardPage(app.clipboard.generation, Ok(page)));
    }
    assert!(!app.clipboard.loading);
    assert!(app.clipboard.error.as_ref().unwrap().contains("Ctrl+R"));

    let _ = app.update(Message::ClipboardRefresh);
    assert!(app.clipboard.loading);
    assert!(app.clipboard.error.is_none());
    assert_eq!(app.clipboard.restarts, 0);
}

#[test]
fn test_clipboard_selection_follows_item_across_refresh() {
    let mut app = test_app(clipboard_addons());
    let generation = open_clipboard(&mut app, "clip ");
    let _ = app.update(Message::ClipboardPage(
        generation,
        Ok(clip_page(
            "a",
            3,
            3,
            vec![clip(0, "a", "a"), clip(1, "b", "b"), clip(2, "c", "c")],
        )),
    ));
    let _ = app.update(Message::MoveDown);
    assert_eq!(app.selected_index, 1);

    let _ = app.update(Message::ClipboardRefresh);
    let _ = app.update(Message::ClipboardPage(
        app.clipboard.generation,
        Ok(clip_page(
            "n",
            4,
            4,
            vec![
                clip(0, "n", "new"),
                clip(1, "a", "a"),
                clip(2, "b", "b"),
                clip(3, "c", "c"),
            ],
        )),
    ));
    assert_eq!(app.selected_index, 2, "still on item b");
}

#[test]
fn test_clipboard_errors_show_as_status_row() {
    let mut app = test_app(clipboard_addons());
    let generation = open_clipboard(&mut app, "clip ");
    let _ = app.update(Message::ClipboardPage(
        generation,
        Err("CopyQ is not running".into()),
    ));
    assert!(!app.clipboard.loading);
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.clipboard.status(), "CopyQ is not running");
    let _ = app.update(Message::Submit);
    assert!(!app.clipboard.pending, "status row does nothing");
    let preview = app.resolve_selected_preview().unwrap();
    assert_eq!(preview.subtitle.as_deref(), Some("CopyQ is not running"));
}

#[test]
fn test_clipboard_previews_are_bound_to_session_and_item() {
    let mut app = test_app(clipboard_addons());
    let generation = open_clipboard(&mut app, "clip ");
    let _ = app.update(Message::ClipboardPage(
        generation,
        Ok(clip_page(
            "a",
            2,
            2,
            vec![clip(0, "a", "a"), clip(1, "b", "b")],
        )),
    ));
    assert_eq!(app.clipboard.preview_inflight.as_deref(), Some("a"));
    let preview = app.resolve_selected_preview().unwrap();
    assert!(matches!(
        preview.bodies.first(),
        Some(PreviewBody::Prose { content, .. }) if content == "Loading preview..."
    ));
    assert!(preview.metadata.iter().any(|(key, _)| key == "Ctrl+Tab"));
    assert!(preview.metadata.iter().any(|(key, _)| key == "Ctrl+R"));

    let session = app.clipboard.session;
    let _ = app.update(Message::ClipboardPreview(
        session.wrapping_sub(1),
        "a".into(),
        Ok(clip_text_preview("wrong")),
    ));
    assert!(app.clipboard.preview("a").is_none());

    // The next preview is requested once the selected one arrives.
    let _ = app.update(Message::MoveDown);
    assert_eq!(app.clipboard.preview_inflight.as_deref(), Some("a"));
    let _ = app.update(Message::ClipboardPreview(
        session,
        "a".into(),
        Ok(clip_text_preview("alpha")),
    ));
    assert_eq!(app.clipboard.preview_inflight.as_deref(), Some("b"));
    let _ = app.update(Message::ClipboardPreview(
        session,
        "b".into(),
        Ok(clip_text_preview("beta\nsecond line")),
    ));
    let preview = app.resolve_selected_preview().unwrap();
    assert!(matches!(
        preview.bodies.first(),
        Some(PreviewBody::Prose { content, .. }) if content == "beta\nsecond line"
    ));

    let mut image = clip_text_preview("");
    image.kind = super::clipboard::ItemKind::Image;
    image.image = Some(Ok(super::clipboard::PreviewImage {
        handle: iced::widget::image::Handle::from_bytes(vec![0u8; 4]),
        mime: "image/png".into(),
        width: 2,
        height: 1,
    }));
    let _ = app.update(Message::ClipboardPreview(session, "b".into(), Ok(image)));
    let preview = app.resolve_selected_preview().unwrap();
    assert!(matches!(
        preview.bodies.first(),
        Some(PreviewBody::ImageData { .. })
    ));

    for index in 0..20 {
        let _ = app.update(Message::ClipboardPreview(
            session,
            format!("other{index}"),
            Ok(clip_text_preview("x")),
        ));
    }
    assert!(app.clipboard.previews.len() <= 8);
}

#[test]
fn test_clipboard_actions_are_captured_and_never_fall_back() {
    let mut app = test_app(clipboard_addons());
    let generation = open_clipboard(&mut app, "clip ");
    let _ = app.update(Message::ClipboardPage(
        generation,
        Ok(clip_page("a", 1, 1, vec![clip(0, "a", "a")])),
    ));

    // Ctrl+Enter pastes by default; without Hyprland it reports why and
    // does not copy instead.
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let _ = app.update(Message::Submit);
    assert!(!app.clipboard.pending);
    assert!(app
        .clipboard
        .error
        .as_ref()
        .unwrap()
        .contains("Hyprland session"));

    app.hyprland = Some(Ok(super::hyprland::Hyprland::test_target()));
    let _ = app.update(Message::Submit);
    assert!(app.clipboard.pending);
    assert!(app.clipboard.error.is_none());
    let session = app.clipboard.session;

    let _ = app.update(Message::ClipboardActionDone(
        session.wrapping_sub(1),
        Ok(Some(super::hyprland::Hyprland::test_target())),
    ));
    assert!(app.clipboard.pending, "late result from another session");
    let _ = app.update(Message::ClipboardActionDone(
        session,
        Err("item is no longer in the clipboard history".into()),
    ));
    assert!(!app.clipboard.pending);
    assert!(app.deferred_action.lock().unwrap().is_none());
    assert!(app.clipboard.error.as_ref().unwrap().contains("no longer"));

    let _ = app.update(Message::Submit);
    let _ = app.update(Message::ClipboardActionDone(
        session,
        Ok(Some(super::hyprland::Hyprland::test_target())),
    ));
    assert!(app.deferred_action.lock().unwrap().is_some());

    // Enter copies and leaves no paste behind.
    let mut app = test_app(clipboard_addons());
    let generation = open_clipboard(&mut app, "clip ");
    let _ = app.update(Message::ClipboardPage(
        generation,
        Ok(clip_page("a", 1, 1, vec![clip(0, "a", "a")])),
    ));
    let _ = app.update(Message::Submit);
    assert!(app.clipboard.pending);
    let _ = app.update(Message::ClipboardActionDone(
        app.clipboard.session,
        Ok(None),
    ));
    assert!(!app.clipboard.pending);
    assert!(app.deferred_action.lock().unwrap().is_none());
}

#[test]
fn test_clipboard_ctrl_tab_cycles_views() {
    use super::state::ClipboardView;
    let mut app = test_app(clipboard_addons());
    open_clipboard(&mut app, "clip ");
    assert_eq!(app.clipboard.view, ClipboardView::All);
    assert_eq!(app.cycle_view_label().as_deref(), Some("All"));

    let generation = app.clipboard.generation;
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.clipboard.view, ClipboardView::Images);
    assert_ne!(
        app.clipboard.generation, generation,
        "rescans with the filter"
    );
    assert_eq!(app.clipboard.status(), "Searching clipboard images...");

    // Typing keeps the view.
    open_clipboard(&mut app, "clip cat");
    assert_eq!(app.clipboard.view, ClipboardView::Images);
    let _ = app.update(Message::ClipboardPage(
        app.clipboard.generation,
        Ok(clip_page("h", 1, 1, Vec::new())),
    ));
    assert_eq!(app.clipboard.status(), "No clipboard images match");

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.clipboard.view, ClipboardView::Urls);
    assert_eq!(app.cycle_view_label().as_deref(), Some("URLs"));
    assert_eq!(app.clipboard.status(), "Searching clipboard URLs...");
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.clipboard.view, ClipboardView::All);
    let _ = app.update(Message::CycleView(true));
    assert_eq!(app.clipboard.view, ClipboardView::Urls);

    // Reopening the browser lists every item again.
    let _ = app.update(Message::SearchChanged("fire".into()));
    assert!(!app.clipboard.active);
    open_clipboard(&mut app, "clip ");
    assert_eq!(app.clipboard.view, ClipboardView::All);
}

#[test]
fn test_classify_query() {
    let addons = clipboard_addons();

    let calc = classify_query("1+1", &addons);
    assert_eq!(calc.mode, QueryMode::Standard);
    assert_eq!(calc.inline_provider, Some(InlineProvider::Calculator));
    assert!(!calc.sensitive);
    assert_eq!(calc.loggable_query(" 1+1 ", &addons), "1+1");

    assert_eq!(
        classify_query("$10 to EUR", &addons).inline_provider,
        Some(InlineProvider::Currency)
    );

    let plain = classify_query("firefox", &addons);
    assert_eq!(plain.inline_provider, None);
    assert!(!plain.sensitive);

    // Inline parsers do not run inside the clipboard browser.
    let clip = classify_query("clip 1+1 secret", &addons);
    assert!(clip.sensitive);
    assert_eq!(clip.inline_provider, None);
    assert_eq!(
        clip.loggable_query("clip 1+1 secret", &addons),
        "clip <redacted>"
    );
    let debug = format!("{clip:?}");
    assert!(!debug.contains("secret"), "{debug}");

    let mut disabled = addons.clone();
    disabled.calculator.enabled = false;
    assert_eq!(classify_query("1+1", &disabled).inline_provider, None);
}

#[test]
fn test_inline_rows_follow_the_query() {
    let mut app = test_app(AddonsConfig::default());
    let _ = app.update(Message::SearchChanged("1+1".into()));
    let inline = app.inline_result.as_ref().expect("calculator row");
    assert_eq!(inline.rows.len(), 1);
    assert_eq!(inline.rows[0].title, "= 1+1 = 2");
    assert_eq!(inline.rows[0].copy_value, "2");
    assert_eq!(app.total_items(), 1);
    assert!(app.resolve_selected_preview().is_some());

    let _ = app.update(Message::SearchChanged("raffi-inline-transition".into()));
    assert!(app.inline_result.is_none());
    assert_eq!(app.total_items(), 0);
    assert!(app.resolve_selected_preview().is_none());

    // A stale calculator row would copy and save the query here.
    let before = app.history.items.clone();
    let _ = app.update(Message::Submit);
    assert_eq!(app.history.items, before);
}

#[test]
fn test_in_flight_actions_survive_query_and_mode_changes() {
    let mut app = test_app(clipboard_addons());
    assert!(app.in_flight.begin("bt", "AA:BB"));

    let _ = app.update(Message::SearchChanged("clip x".into()));
    let _ = app.update(Message::SearchChanged("firefox".into()));
    let _ = app.update(Message::ToggleHelp);
    let _ = app.update(Message::ToggleHelp);
    assert!(app.in_flight.is_pending("bt", "AA:BB"));
    assert!(!app.in_flight.begin("bt", "AA:BB"));
    assert!(app.in_flight.begin("bt", "CC:DD"));

    app.in_flight.finish("bt", "AA:BB");
    assert!(!app.in_flight.is_pending("bt", "AA:BB"));
    assert!(app.in_flight.is_pending("bt", "CC:DD"));
    assert!(app.in_flight.begin("bt", "AA:BB"));
}

#[test]
fn test_cancel_queues_no_deferred_action() {
    let mut app = test_app(AddonsConfig::default());
    let _ = app.update(Message::SearchChanged("1+1".into()));
    let _ = app.update(Message::Cancel);
    assert!(app.deferred_action.lock().unwrap().is_none());
}

fn failing_copier() -> Option<Vec<String>> {
    Some(vec![
        "sh".into(),
        "-c".into(),
        "cat >/dev/null; exit 1".into(),
    ])
}

#[test]
fn test_inline_copy_failure_keeps_launcher_open() {
    let mut app = test_app(AddonsConfig::default());
    app.copier = failing_copier();
    let _ = app.update(Message::SearchChanged("1+1".into()));
    let before = app.history.items.clone();

    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    assert_eq!(app.update(Message::Submit).units(), 0);

    let result = app.copy_job(app.inline_copy_value(0).unwrap())();
    let task = app.update(Message::InlineCopied(app.inline_generation, result));
    assert_eq!(task.units(), 0);
    assert!(!app.in_flight.is_pending("inline-copy", ""));
    assert_eq!(
        app.inline_copy_error.as_deref(),
        Some("Copy failed: sh failed (exit 1)")
    );
    // The error row is not selectable.
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.history.items, before);

    // Retrying clears the error and starts a new copy.
    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    assert!(app.inline_copy_error.is_none());
}

#[test]
fn test_inline_copy_uses_alt_value_with_ctrl() {
    let mut app = test_app(AddonsConfig::default());
    app.copier = Some(vec![
        "sh".into(),
        "-c".into(),
        "test \"$(cat)\" = alt".into(),
    ]);
    app.inline_result = Some(super::types::InlineResult {
        provider: InlineProvider::Calculator,
        rows: vec![super::types::InlineRow {
            title: "row".into(),
            subtitle: None,
            copy_value: "plain".into(),
            alt_copy_value: Some("alt".into()),
            swatch: None,
            preview: None,
        }],
    });

    assert!(app.copy_job(app.inline_copy_value(0).unwrap())().is_err());
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    assert!(app.copy_job(app.inline_copy_value(0).unwrap())().is_ok());
}

#[test]
fn test_stale_inline_copy_releases_guard_without_touching_new_query() {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("1+1".into()));
    let _ = app.update(Message::Submit);
    let started = app.inline_generation;
    let _ = app.update(Message::SearchChanged("2+2".into()));
    assert!(app.in_flight.is_pending("inline-copy", ""));

    let task = app.update(Message::InlineCopied(started, Err("boom".into())));
    assert_eq!(task.units(), 0);
    assert!(!app.in_flight.is_pending("inline-copy", ""));
    assert!(app.inline_copy_error.is_none());

    // A stale success neither closes Raffi nor saves the new query, even
    // when the query returns to the one that started the copy.
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::SearchChanged("1+1".into()));
    let before = app.history.items.clone();
    let task = app.update(Message::InlineCopied(started, Ok(())));
    assert_eq!(task.units(), 0);
    assert!(!app.in_flight.is_pending("inline-copy", ""));
    assert_eq!(app.history.items, before);

    // The current generation still closes Raffi.
    let _ = app.update(Message::Submit);
    let task = app.update(Message::InlineCopied(app.inline_generation, Ok(())));
    assert!(task.units() > 0);
}

/// Submits the selected currency row with a failing copier, checks the
/// error row, then that a retry clears it and starts a new copy.
fn assert_currency_copy_failure_and_retry(app: &mut LauncherApp) {
    app.copier = failing_copier();
    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    assert_eq!(app.update(Message::Submit).units(), 0);

    let result = app.copy_job("9.25".into())();
    let task = app.update(Message::InlineCopied(app.inline_generation, result));
    assert_eq!(task.units(), 0);
    assert!(!app.in_flight.is_pending("inline-copy", ""));
    assert_eq!(
        app.inline_copy_error.as_deref(),
        Some("Copy failed: sh failed (exit 1)")
    );

    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    assert!(app.inline_copy_error.is_none());
}

#[test]
fn test_currency_copy_failure_keeps_launcher_open() {
    let mut app = test_app(AddonsConfig::default());
    app.currency.result = Some(CurrencyResult {
        request: CurrencyConversionRequest {
            amount: 10.0,
            from_currency: "USD".into(),
            to_currency: "EUR".into(),
        },
        converted_amount: 9.254,
        rate: 0.9254,
    });
    assert_currency_copy_failure_and_retry(&mut app);
}

#[test]
fn test_multi_currency_copy_failure_keeps_launcher_open() {
    let mut app = test_app(AddonsConfig::default());
    app.currency.multi_result = Some(super::types::MultiCurrencyResult {
        amount: 10.0,
        from_currency: "USD".into(),
        conversions: vec![
            super::types::CurrencyConversion {
                to_currency: "EUR".into(),
                converted_amount: 9.25,
                rate: 0.925,
            },
            super::types::CurrencyConversion {
                to_currency: "GBP".into(),
                converted_amount: 7.891,
                rate: 0.7891,
            },
        ],
    });
    app.selected_index = 1;
    assert_currency_copy_failure_and_retry(&mut app);
}

#[test]
fn test_calculator_owns_inline_area_over_currency() {
    let mut addons = AddonsConfig::default();
    addons.currency.trigger = Some("-".into());
    assert!(super::currency::try_parse_multi_currency_conversion("-10", &[], "USD", "-").is_some());
    assert_eq!(
        classify_query("-10", &addons).inline_provider,
        Some(InlineProvider::Calculator)
    );

    let mut app = test_app(addons);
    let _ = app.update(Message::SearchChanged("-10".into()));
    assert!(app.inline_result.is_some());
    assert!(!app.currency.loading && app.currency.result.is_none());
    assert!(!app.currency.multi_loading && app.currency.multi_result.is_none());
    assert_eq!(app.total_items(), 1);

    let _ = app.update(Message::SearchChanged("-".into()));
    assert!(app.currency.help);
}

#[test]
fn test_unit_conversion_inline_row() {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("10 km to mi".into()));
    assert_eq!(app.query_class.inline_provider, Some(InlineProvider::Units));
    assert!(!app.query_class.sensitive);
    let inline = app.inline_result.as_ref().expect("units row");
    assert_eq!(inline.rows[0].title, "10 km = 6.21371 mi");
    assert_eq!(app.total_items(), 1);
    assert!(app.resolve_selected_preview().is_some());

    assert_eq!(
        app.inline_copy_value(0).as_deref(),
        Some("6.21371192237334")
    );
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    assert_eq!(
        app.inline_copy_value(0).as_deref(),
        Some("6.21371192237334 mi")
    );
    app.view.current_modifiers = iced::keyboard::Modifiers::default();

    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    let task = app.update(Message::InlineCopied(app.inline_generation, Ok(())));
    assert!(task.units() > 0);

    let _ = app.update(Message::SearchChanged("10 km".into()));
    assert!(app.inline_result.is_none());
    let _ = app.update(Message::SearchChanged("raffi-units-transition".into()));
    assert!(app.inline_result.is_none());
    assert_eq!(app.total_items(), 0);
}

#[test]
fn test_unit_conversion_leaves_math_and_currency_alone() {
    let addons = AddonsConfig::default();
    assert_eq!(
        classify_query("2*3", &addons).inline_provider,
        Some(InlineProvider::Calculator)
    );
    assert_eq!(
        classify_query("$10 usd to eur", &addons).inline_provider,
        Some(InlineProvider::Currency)
    );
    for query in ["10 usd to eur", "10 apples", "10 to", "5 m", "firefox"] {
        assert_eq!(
            classify_query(query, &addons).inline_provider,
            None,
            "{query}"
        );
    }

    // With an empty currency trigger, currency still wins its own queries.
    let mut bare = addons.clone();
    bare.currency.trigger = Some(String::new());
    assert_eq!(
        classify_query("10 usd to eur", &bare).inline_provider,
        Some(InlineProvider::Currency)
    );
    assert_eq!(
        classify_query("10 km to mi", &bare).inline_provider,
        Some(InlineProvider::Units)
    );

    let mut disabled = addons;
    disabled.units.enabled = false;
    assert_eq!(
        classify_query("10 km to mi", &disabled).inline_provider,
        None
    );
}

#[test]
fn test_color_inline_rows_copy_selected_format() {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("#ff8800".into()));
    assert_eq!(app.query_class.inline_provider, Some(InlineProvider::Color));
    assert!(!app.query_class.sensitive);
    let inline = app.inline_result.as_ref().expect("color rows");
    let titles: Vec<&str> = inline.rows.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "#ff8800",
            "rgb(255, 136, 0)",
            "hsl(32, 100%, 50%)",
            "nearest: darkorange"
        ]
    );
    assert!(inline.rows.iter().all(|row| row.swatch.is_some()));
    assert_eq!(app.total_items(), 4);

    let _ = app.update(Message::MoveDown);
    assert_eq!(app.selected_index, 1);
    assert!(app.resolve_selected_preview().is_some());
    assert_eq!(
        app.inline_copy_value(app.selected_index).as_deref(),
        Some("rgb(255, 136, 0)")
    );
    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));

    let _ = app.update(Message::SearchChanged("#fff".into()));
    assert_eq!(app.inline_result.as_ref().map(|r| r.rows.len()), Some(4));
    let _ = app.update(Message::SearchChanged("firefox".into()));
    assert!(app.inline_result.is_none());
    assert_eq!(app.total_items(), 0);
}

#[test]
fn test_color_inline_provider_order_and_rejections() {
    let addons = AddonsConfig::default();
    for (query, provider) in [
        ("#123", Some(InlineProvider::Color)),
        ("rgb(1 2 3)", Some(InlineProvider::Color)),
        ("hsl(32, 100%, 50%)", Some(InlineProvider::Color)),
        ("2*3", Some(InlineProvider::Calculator)),
        ("10 km to mi", Some(InlineProvider::Units)),
        ("#", None),
        ("#zzz", None),
        ("red", None),
        ("color", None),
        ("firefox", None),
    ] {
        assert_eq!(
            classify_query(query, &addons).inline_provider,
            provider,
            "{query}"
        );
    }

    let mut disabled = addons;
    disabled.color.enabled = false;
    assert_eq!(classify_query("#ff8800", &disabled).inline_provider, None);
}

#[test]
fn test_world_clock_inline_rows_copy_time_and_timestamp() {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("time in tokyo".into()));
    assert_eq!(app.query_class.inline_provider, Some(InlineProvider::Time));
    assert!(!app.query_class.sensitive);
    let inline = app.inline_result.as_ref().expect("time row");
    assert_eq!(inline.rows.len(), 1);
    let row = inline.rows[0].clone();
    assert!(row.subtitle.as_deref().unwrap().contains("Asia/Tokyo"));
    assert!(app.resolve_selected_preview().is_some());
    assert_eq!(app.inline_copy_value(0), Some(row.title.clone()));
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let iso = app.inline_copy_value(0).unwrap();
    assert!(iso.ends_with("+09:00"), "{iso}");
    app.view.current_modifiers = iced::keyboard::Modifiers::empty();

    let _ = app.update(Message::SearchChanged("time eastern".into()));
    assert_eq!(app.inline_result.as_ref().map(|r| r.rows.len()), Some(2));
    let _ = app.update(Message::SearchChanged("timeshift".into()));
    assert!(app.inline_result.is_none());
}

#[test]
fn test_world_clock_provider_order_and_rejections() {
    let addons = AddonsConfig::default();
    for (query, provider) in [
        ("time tokyo", Some(InlineProvider::Time)),
        ("time nyc", Some(InlineProvider::Time)),
        ("now in paris", Some(InlineProvider::Time)),
        ("15:00 paris to nyc", Some(InlineProvider::Time)),
        ("3pm paris in tokyo", Some(InlineProvider::Time)),
        ("10 km to mi", Some(InlineProvider::Units)),
        ("2*3", Some(InlineProvider::Calculator)),
        ("time", None),
        ("timeshift", None),
        ("time foo", None),
        ("15:00 paris", None),
    ] {
        assert_eq!(
            classify_query(query, &addons).inline_provider,
            provider,
            "{query}"
        );
    }

    let mut disabled = addons;
    disabled.world_clock.enabled = false;
    assert_eq!(
        classify_query("time tokyo", &disabled).inline_provider,
        None
    );
}

#[test]
fn test_status_inline_row_is_not_copied() {
    let mut app = test_app(AddonsConfig::default());
    app.inline_result = Some(InlineResult {
        provider: InlineProvider::Time,
        rows: vec![InlineRow {
            title: "Time zone data not found (install tzdata)".into(),
            subtitle: None,
            copy_value: String::new(),
            alt_copy_value: None,
            swatch: None,
            preview: None,
        }],
    });
    assert_eq!(app.inline_copy_value(0), None);
    let _ = app.update(Message::Submit);
    assert!(!app.in_flight.is_pending("inline-copy", ""));
}

fn open_system(app: &mut LauncherApp, query: &str) {
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged(query.into()));
    assert_eq!(app.command_list.addon, Some(CommandListAddon::System));
}

fn deferred_argv(app: &LauncherApp) -> Option<Vec<String>> {
    match app.deferred_action.lock().unwrap().as_ref()? {
        DeferredAction::Command(argv) => Some(argv.clone()),
        other => panic!("unexpected deferred action {other:?}"),
    }
}

fn command_list_titles(app: &LauncherApp) -> Vec<&str> {
    app.command_list
        .filtered
        .iter()
        .map(|&index| app.command_list.items[index].title.as_str())
        .collect()
}

#[test]
fn test_route_query_system_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(route_query("sys", &addons), QueryMode::Standard);
    assert_eq!(
        route_query("sys ", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::System,
            query: String::new()
        }
    );
    assert_eq!(
        route_query(" sys reb", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::System,
            query: "reb".into()
        }
    );

    addons.system.trigger = Some("power".into());
    assert_eq!(route_query("sys reb", &addons), QueryMode::Standard);
    assert!(matches!(
        route_query("power reb", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::System,
            ..
        }
    ));

    addons.system.enabled = false;
    assert_eq!(route_query("power reb", &addons), QueryMode::Standard);
    assert!(keyword_suggestions("power", &addons).is_empty());
}

#[test]
fn test_default_triggers_do_not_shadow_each_other_or_web_searches() {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = Some(true);
    assert!(matches!(
        route_query("emoji x", &addons),
        QueryMode::Emoji { .. }
    ));
    assert!(matches!(
        route_query("clip x", &addons),
        QueryMode::Clipboard { .. }
    ));
    assert!(matches!(
        route_query("sys x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::System,
            ..
        }
    ));
    assert!(matches!(
        route_query("win x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Windows,
            ..
        }
    ));
    assert!(matches!(
        route_query("media x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Media,
            ..
        }
    ));
    assert!(matches!(
        route_query("audio x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Audio,
            ..
        }
    ));
    assert!(matches!(
        route_query("kill x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Kill,
            ..
        }
    ));
    assert!(matches!(
        route_query("bt x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Bluetooth,
            ..
        }
    ));

    // A user web search keeps its keyword, and its suggestion comes first.
    addons.web_searches.push(WebSearchConfig {
        name: "Sysdig".into(),
        keyword: "sys".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
        ..Default::default()
    });
    assert!(matches!(
        route_query("sys x", &addons),
        QueryMode::WebSearch { .. }
    ));
    let subtitles: Vec<String> = keyword_suggestions("sys", &addons)
        .into_iter()
        .map(|suggestion| suggestion.subtitle)
        .collect();
    assert_eq!(
        subtitles,
        vec![
            "Search Sysdig",
            "Lock, suspend, hibernate, log out, reboot or shut down"
        ]
    );
}

#[test]
fn test_system_list_filters_entries() {
    let mut app = test_app(AddonsConfig::default());
    open_system(&mut app, "sys ");
    assert_eq!(
        command_list_titles(&app),
        vec![
            "Lock",
            "Suspend",
            "Hibernate",
            "Log out",
            "Reboot",
            "Shut down"
        ]
    );
    assert_eq!(app.total_items(), 6);
    assert!(app.filtered_configs.is_empty());
    assert_eq!(app.fallback_count(), 0);

    open_system(&mut app, "sys reb");
    assert_eq!(command_list_titles(&app), vec!["Reboot"]);
    open_system(&mut app, "sys zzz");
    assert_eq!(app.total_items(), 0);

    let _ = app.update(Message::SearchChanged("firefox".into()));
    assert!(!app.command_list.is_active());
    assert!(app.command_list.items.is_empty());
}

#[test]
fn test_system_lock_runs_on_first_enter() {
    let mut app = test_app(AddonsConfig::default());
    open_system(&mut app, "sys lock");
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes");
    assert_eq!(
        deferred_argv(&app),
        Some(vec!["loginctl".into(), "lock-session".into()])
    );
}

#[test]
fn test_system_power_actions_need_a_second_enter() {
    let mut app = test_app(AddonsConfig::default());
    open_system(&mut app, "sys reboot");

    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0);
    assert!(app.command_list.armed.is_some());
    assert!(app.deferred_action.lock().unwrap().is_none());

    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes");
    assert_eq!(
        deferred_argv(&app),
        Some(vec!["systemctl".into(), "reboot".into()])
    );
}

#[test]
fn test_system_confirmation_resets_on_selection_query_and_modifiers() {
    let mut app = test_app(AddonsConfig::default());
    open_system(&mut app, "sys ");
    app.selected_index = 4;

    let _ = app.update(Message::Submit);
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::MoveUp);
    assert!(app.command_list.armed.is_none());
    let _ = app.update(Message::Submit);
    assert!(app.command_list.armed.is_some());

    let _ = app.update(Message::SearchChanged("sys re".into()));
    assert!(app.command_list.armed.is_none());
    let _ = app.update(Message::Submit);

    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    assert!(app.command_list.armed.is_none());
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::empty()));
    let _ = app.update(Message::Submit);
    assert!(app.command_list.armed.is_some());

    // Clicking another row arms that row instead of running the first.
    let _ = app.update(Message::SearchChanged("sys ".into()));
    let _ = app.update(Message::ItemClicked(4));
    let _ = app.update(Message::ItemClicked(5));
    assert_eq!(
        app.command_list
            .armed
            .as_ref()
            .map(|armed| armed.identity.as_str()),
        Some("shutdown")
    );

    let _ = app.update(Message::ToggleHelp);
    assert!(app.command_list.armed.is_none());
    assert!(app.deferred_action.lock().unwrap().is_none());
}

#[test]
fn test_system_ignores_enter_autorepeat() {
    let mut app = test_app(AddonsConfig::default());
    open_system(&mut app, "sys shut");
    let _ = app.update(Message::Submit);
    assert_eq!(app.update(Message::SubmitRepeat).units(), 0);
    assert_eq!(app.update(Message::SubmitRepeat).units(), 0);
    assert!(app.deferred_action.lock().unwrap().is_none());

    open_system(&mut app, "sys lock");
    assert_eq!(app.update(Message::SubmitRepeat).units(), 0);
    assert!(app.deferred_action.lock().unwrap().is_none());

    open_system(&mut app, "sys shut");
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::Submit);
    assert_eq!(
        deferred_argv(&app),
        Some(vec!["systemctl".into(), "poweroff".into()])
    );
}

#[test]
fn test_system_commands_overrides_and_hide() {
    let mut addons = AddonsConfig::default();
    addons.system.commands.lock = Some(vec!["hyprlock".into(), "--immediate".into()]);
    addons.system.commands.suspend = Some(Vec::new());
    addons.system.commands.logout = Some(vec!["my-logout".into()]);
    addons.system.hide = vec![crate::SystemCommand::Hibernate];

    let entries = system_entries(&addons.system, Some(Compositor::Hyprland), None, |_| true);
    let ids: Vec<crate::SystemCommand> = entries.iter().map(|entry| entry.id).collect();
    use crate::SystemCommand::*;
    assert_eq!(ids, vec![Lock, Logout, Reboot, Shutdown]);
    assert!(matches!(&entries[1].action, DeferredAction::Command(argv) if argv == &["my-logout"]));

    let mut app = test_app(addons);
    open_system(&mut app, "sys lock");
    let _ = app.update(Message::Submit);
    assert_eq!(
        deferred_argv(&app),
        Some(vec!["hyprlock".into(), "--immediate".into()])
    );
}

#[test]
fn test_system_logout_default_follows_session() {
    let config = crate::SystemAddonConfig::default();
    let logout = |compositor, session| {
        system_entries(&config, compositor, session, |_| true)
            .into_iter()
            .find(|entry| entry.id == crate::SystemCommand::Logout)
            .unwrap()
            .action
    };
    assert!(matches!(
        logout(Some(Compositor::Hyprland), Some("3")),
        DeferredAction::Hypr(HyprAction::Exit)
    ));
    let hyprland_logout = system_entries(&config, Some(Compositor::Hyprland), None, |_| true)
        .into_iter()
        .find(|entry| entry.id == crate::SystemCommand::Logout)
        .unwrap();
    assert_eq!(
        hyprland_logout.command_line(),
        "Hyprland exit dispatcher via hyprctl"
    );
    assert!(matches!(
        logout(Some(Compositor::Niri), None),
        DeferredAction::NiriQuit
    ));
    let niri_logout = system_entries(&config, Some(Compositor::Niri), None, |program| {
        program != "niri"
    })
    .into_iter()
    .find(|entry| entry.id == crate::SystemCommand::Logout)
    .unwrap();
    assert_eq!(niri_logout.missing.as_deref(), Some("niri"));
    assert!(matches!(
        logout(Some(Compositor::Sway), None),
        DeferredAction::SwayExit
    ));
    let sway_logout = system_entries(&config, Some(Compositor::Sway), None, |program| {
        program != "swaymsg"
    })
    .into_iter()
    .find(|entry| entry.id == crate::SystemCommand::Logout)
    .unwrap();
    assert_eq!(sway_logout.missing.as_deref(), Some("swaymsg"));
    assert_eq!(sway_logout.command_line(), "swaymsg exit");
    assert!(matches!(
        logout(None, Some("3")),
        DeferredAction::Command(argv) if argv == ["loginctl", "terminate-session", "3"]
    ));
}

#[test]
fn test_vault_queries_are_saved_in_history_but_not_logged() {
    let addons = AddonsConfig::default();
    for query in ["bw github", "pass web/github"] {
        let class = classify_query(query, &addons);
        assert!(class.saved_in_history(), "{query}");
        assert_eq!(loggable_query(query, &addons), "<redacted>");
    }
    assert!(!classify_query("sys reboot", &addons).saved_in_history());
    assert!(!classify_query("clip secret", &clipboard_addons()).saved_in_history());
    assert!(classify_query("firefox", &addons).saved_in_history());
}

#[test]
fn test_system_queries_stay_out_of_history_and_logs() {
    let addons = AddonsConfig::default();
    assert_eq!(loggable_query("sys reboot", &addons), "<redacted>");
    assert!(classify_query("sys reboot", &addons).sensitive);

    let mut app = test_app(addons);
    open_system(&mut app, "sys lock");
    app.history.max_items = 10;
    let before = app.history.items.clone();
    let _ = app.update(Message::Submit);
    app.save_query_to_history();
    assert_eq!(app.history.items, before);
}

#[test]
fn test_system_missing_program_is_not_actionable() {
    let mut addons = AddonsConfig::default();
    addons.system.commands.lock = Some(vec!["no-such-locker".into()]);
    let entries = system_entries(&addons.system, None, Some("2"), |program| {
        program != "no-such-locker"
    });
    let lock = entries
        .iter()
        .find(|entry| entry.id == crate::SystemCommand::Lock)
        .unwrap();
    assert_eq!(lock.missing.as_deref(), Some("no-such-locker"));
    assert!(entries
        .iter()
        .filter(|entry| entry.id != crate::SystemCommand::Lock)
        .all(|entry| entry.missing.is_none()));

    let mut app = test_app(addons);
    app.program_exists = |program| program != "no-such-locker";
    open_system(&mut app, "sys lock");
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::Submit);
    assert_eq!(deferred_argv(&app), None);
}

#[test]
fn test_system_click_elsewhere_cancels_confirmation() {
    let mut addons = AddonsConfig::default();
    addons.system.commands.lock = Some(vec!["no-such-locker".into()]);
    let mut app = test_app(addons);
    app.program_exists = |program| program != "no-such-locker";
    open_system(&mut app, "sys ");
    let position = |app: &LauncherApp, id| {
        app.command_list
            .filtered
            .iter()
            .position(|&index| app.command_list.items[index].identity == id)
            .unwrap()
    };
    let reboot = position(&app, "reboot");
    let lock = position(&app, "lock");

    let _ = app.update(Message::ItemClicked(reboot));
    assert!(app.command_list.armed.is_some());
    let _ = app.update(Message::ItemClicked(lock));
    assert!(app.command_list.armed.is_none());
    let _ = app.update(Message::ItemClicked(reboot));
    assert_eq!(deferred_argv(&app), None);
    assert!(app.command_list.armed.is_some());
}

#[test]
fn test_system_logout_without_session_is_not_actionable() {
    let config = crate::SystemAddonConfig::default();
    let logout = system_entries(&config, None, None, |_| true)
        .into_iter()
        .find(|entry| entry.id == crate::SystemCommand::Logout)
        .unwrap();
    assert_eq!(logout.missing.as_deref(), Some("XDG_SESSION_ID"));

    let mut config = crate::SystemAddonConfig::default();
    config.commands.logout = Some(vec!["uwsm".into(), "stop".into()]);
    let logout = system_entries(&config, None, None, |_| true)
        .into_iter()
        .find(|entry| entry.id == crate::SystemCommand::Logout)
        .unwrap();
    assert!(logout.missing.is_none());
}

const WINDOW_CLIENTS: &str = r#"[
    {"address": "0xa1", "mapped": true, "hidden": false,
     "workspace": {"id": 3, "name": "3"}, "class": "firefox",
     "title": "Mozilla Firefox", "initialClass": "firefox", "pid": 100,
     "focusHistoryID": 2},
    {"address": "0xb2", "mapped": true, "hidden": false,
     "workspace": {"id": 1, "name": "1"}, "class": "kitty",
     "title": "notes", "initialClass": "kitty", "pid": 101,
     "focusHistoryID": 1}
]"#;

fn window_fixture() -> Vec<super::command_list::CommandItem> {
    let monitors = br#"[{"focused": true, "activeWorkspace": {"id": 2, "name": "2"}}]"#;
    super::windows::hyprland_items(WINDOW_CLIENTS.as_bytes(), monitors, 1).unwrap()
}

fn windows_app() -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.compositor = || Some(Compositor::Hyprland);
    app.history.max_items = 0;
    app
}

fn deferred_action(app: &LauncherApp) -> Option<DeferredAction> {
    app.deferred_action.lock().unwrap().clone()
}

#[test]
fn test_route_query_windows_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("win fire", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Windows,
            query: "fire".into()
        }
    );
    assert!(classify_query("win fire", &addons).sensitive);
    assert_eq!(loggable_query("win fire", &addons), "<redacted>");
    assert_eq!(
        keyword_suggestions("win", &addons)[0].subtitle,
        "Switch to an open window"
    );

    addons.windows.enabled = false;
    assert_eq!(route_query("win fire", &addons), QueryMode::Standard);
}

#[test]
fn test_windows_load_then_filter_and_focus() {
    let mut app = windows_app();
    let task = app.update(Message::SearchChanged("win ".into()));
    assert!(task.units() > 0, "the window list loads");
    assert_eq!(app.command_list.status(), Some("Loading windows..."));
    assert_eq!(app.total_items(), 1);
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());

    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    assert_eq!(command_list_titles(&app), vec!["notes", "Mozilla Firefox"]);

    // Narrowing the query filters the loaded list without loading again.
    let task = app.update(Message::SearchChanged("win fire".into()));
    assert!(task.units() > 0, "the list scrolls to the first match");
    assert_eq!(app.command_list.generation, generation);
    assert_eq!(command_list_titles(&app), vec!["Mozilla Firefox"]);

    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes");
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Hypr(HyprAction::Focus { address })) if address == "0xa1"
    ));
    let before = app.history.items.clone();
    app.history.max_items = 10;
    app.save_query_to_history();
    assert_eq!(app.history.items, before);
}

#[test]
fn test_select_1_waits_for_async_results_and_activates_the_only_selectable_row() {
    let mut app = windows_app();
    app.select_1_query = Some("win ".into());
    let _ = app.update(Message::SearchChanged("win ".into()));
    assert_eq!(app.command_list.status(), Some("Loading windows..."));
    assert_eq!(app.select_1_query.as_deref(), Some("win "));
    assert!(deferred_action(&app).is_none());

    let generation = app.command_list.generation;
    let mut items: Vec<_> = window_fixture()
        .into_iter()
        .filter(|item| item.title == "Mozilla Firefox")
        .collect();
    items.push(super::command_list::CommandItem::info(
        "notice",
        "Not selectable".into(),
        "dialog-information",
    ));
    let _ = app.update(Message::CommandListLoaded(generation, Ok(items)));

    assert_eq!(app.total_items(), 2);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Hypr(HyprAction::Focus { address })) if address == "0xa1"
    ));
    assert!(app.select_1_query.is_none());

    *app.deferred_action.lock().unwrap() = None;
    let _ = app.update(Message::WindowResized(iced::Size::new(800.0, 600.0)));
    assert!(deferred_action(&app).is_none());
}

#[test]
fn test_select_1_does_not_activate_after_the_initial_query_changes() {
    let mut app = windows_app();
    app.select_1_query = Some("win fire".into());
    let _ = app.update(Message::SearchChanged("win fire".into()));
    let generation = app.command_list.generation;

    let _ = app.update(Message::SearchChanged("win notes".into()));
    assert!(app.select_1_query.is_none());
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));

    assert!(deferred_action(&app).is_none());
}

#[test]
fn test_select_1_requires_a_non_empty_initial_query_and_is_opt_in() {
    let mut app = windows_app();
    app.select_1_query = Some(String::new());
    let _ = app.update(Message::SearchChanged(String::new()));
    assert!(app.select_1_query.is_none());

    let _ = app.update(Message::SearchChanged("win fire".into()));
    let generation = app.command_list.generation;
    let items: Vec<_> = window_fixture()
        .into_iter()
        .filter(|item| item.title == "Mozilla Firefox")
        .collect();
    let _ = app.update(Message::CommandListLoaded(generation, Ok(items)));

    assert!(deferred_action(&app).is_none());
}

fn alias(description: &str, alias: &str) -> RaffiConfig {
    RaffiConfig {
        description: Some(description.into()),
        alias: Some(alias.into()),
        ..Default::default()
    }
}

fn alias_app(configs: Vec<RaffiConfig>, addons: AddonsConfig) -> LauncherApp {
    let (mut app, _) = LauncherApp::new(
        configs,
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
    app.text_snippets.mru_map.clear();
    app.text_snippets.mru_path = None;
    app.program_exists = |_| true;
    app.history.max_items = 0;
    app
}

fn launcher_keyword(keyword: &str, alias: &str) -> crate::LauncherKeyword {
    crate::LauncherKeyword {
        keyword: keyword.into(),
        alias: alias.into(),
        description: format!("{keyword} launcher"),
        icon: None,
    }
}

/// An `mp` pull request script filter and a `linoux` keyword for one repo.
fn pull_request_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "My Pull Requests".into(),
        command: "pr".into(),
        keyword: "mp".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
        dynamic_preview: None,
        ..Default::default()
    });
    addons
        .launcher_keywords
        .push(launcher_keyword("linoux", "mp linus/linux"));
    addons
}

fn script_filter_query(query: &str, addons: &AddonsConfig) -> Option<String> {
    match classify_query(query, addons).mode {
        QueryMode::ScriptFilter { query, .. } => Some(query),
        _ => None,
    }
}

#[test]
fn test_launcher_keyword_routes_as_its_alias() {
    let addons = pull_request_addons();
    for (typed, routed) in [
        ("linoux torvalds", Some("linus/linux torvalds")),
        (" linoux ", Some("linus/linux")),
        ("linoux", None),
        ("linouxx torvalds", None),
        ("Linoux torvalds", None),
    ] {
        assert_eq!(
            script_filter_query(typed, &addons).as_deref(),
            routed,
            "{typed:?}"
        );
    }
}

#[test]
fn test_launcher_keyword_expands_once_and_wins_over_addon_keywords() {
    let mut addons = pull_request_addons();
    addons.launcher_keywords = vec![
        launcher_keyword("mp", "mp chmouel/raffi"),
        launcher_keyword("again", "linoux"),
    ];
    assert_eq!(
        script_filter_query("mp fix", &addons).as_deref(),
        Some("chmouel/raffi fix")
    );
    assert_eq!(classify_query("again x", &addons).mode, QueryMode::Standard);
}

#[test]
fn test_launcher_keyword_to_clipboard_stays_out_of_logs() {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = Some(true);
    let trigger = addons.clipboard.trigger().to_string();
    addons
        .launcher_keywords
        .push(launcher_keyword("cc", &format!("{trigger} ")));
    assert!(classify_query("cc secret", &addons).sensitive);
    assert_eq!(
        loggable_query("cc secret", &addons),
        format!("{trigger} <redacted>")
    );
}

#[test]
fn test_launcher_keyword_keeps_the_typed_text_and_filters_loaded_snippets() {
    let mut source = snippet_source("Snippets", "sn", Vec::new());
    source.snippets = None;
    source.command = Some("snippets-command".into());
    let mut addons = AddonsConfig::default();
    addons.emoji.enabled = false;
    addons.text_snippets.push(source);
    addons.launcher_keywords.push(launcher_keyword("q", "sn"));
    let mut app = test_app(addons);
    open_snippets(&mut app, "q quo");
    assert!(app.text_snippets.loading);

    let generation = app.text_snippets.generation;
    let _ = app.update(Message::TextSnippetCommandResult(
        generation,
        Ok(vec![snippet("Quote", "> x"), snippet("Hello", "hi")]),
    ));
    assert_eq!(app.search_query, "q quo");
    assert_eq!(app.text_snippets.filtered, vec![0]);
}

#[test]
fn test_launcher_keywords_are_suggested() {
    let mut addons = pull_request_addons();
    addons.launcher_keywords[0].icon = Some("github".into());
    let suggestions = keyword_suggestions("linoux", &addons);
    assert_eq!(suggestions[0].keyword, "linoux");
    assert_eq!(suggestions[0].subtitle, "linoux launcher");
    assert_eq!(suggestions[0].icon.as_deref(), Some("github"));
}

#[test]
fn test_picking_an_alias_launcher_only_types_its_keyword_or_alias() {
    let mut linoux = alias("Linus Pull Requests", "mp linus/linux");
    linoux.keyword = Some("linoux".into());
    let mut app = alias_app(
        vec![linoux, alias("Volume up", "audio volume up")],
        pull_request_addons(),
    );

    let _ = app.update(Message::SearchChanged("Linus Pull".into()));
    app.view.current_modifiers = iced::keyboard::Modifiers::ALT;
    app.selected_index = 0;
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "linoux ");
    assert_eq!(app.view.current_modifiers, iced::keyboard::Modifiers::ALT);
    assert!(matches!(
        &app.query_class.mode,
        QueryMode::ScriptFilter { query, .. } if query == "linus/linux"
    ));
    assert!(app.selected_item.lock().unwrap().is_none());

    app.view.current_modifiers = iced::keyboard::Modifiers::default();
    let _ = app.update(Message::SearchChanged("Volume up".into()));
    app.selected_index = 0;
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "audio volume up");
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::audio::fixture_items(true)),
    ));
    assert!(app
        .command_list
        .items
        .iter()
        .all(|item| !app.in_flight.is_pending("audio", &item.identity)));
    assert!(deferred_action(&app).is_none());
    assert!(app.selected_item.lock().unwrap().is_none());
}

#[test]
fn test_select_1_does_not_activate_zero_or_multiple_selectable_rows() {
    let mut app = windows_app();
    app.select_1_query = Some("win ".into());
    let _ = app.update(Message::SearchChanged("win ".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    assert!(deferred_action(&app).is_none());
    assert!(app.select_1_query.is_none());

    let mut app = windows_app();
    app.select_1_query = Some("win no-such-window".into());
    let _ = app.update(Message::SearchChanged("win no-such-window".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    assert!(deferred_action(&app).is_none());
    assert!(app.select_1_query.is_none());
}

#[test]
fn test_select_1_waits_for_bitwarden_notes_search() {
    let mut app = test_app(AddonsConfig::default());
    app.search_query = "bw recovery".into();
    app.query_class = super::app::classify_query(&app.search_query, &app.addons);
    app.select_1_query = Some(app.search_query.clone());
    app.select_1_ready = true;
    app.command_list.enter(CommandListAddon::Bitwarden);
    app.command_list.set_query("recovery".into());
    app.vault.search_query = Some("recovery".into());

    let _ = app.update(Message::WindowResized(iced::Size::new(800.0, 600.0)));

    assert_eq!(app.select_1_query.as_deref(), Some("bw recovery"));
}

#[test]
fn test_select_1_indexes_results_after_currency_help() {
    let mut app = test_app(AddonsConfig::default());
    app.search_query = "x".into();
    app.query_class = classify_query(&app.search_query, &app.addons);
    app.currency.help = true;
    app.inline_result = Some(InlineResult {
        provider: InlineProvider::Units,
        rows: vec![InlineRow {
            title: "one result".into(),
            subtitle: None,
            copy_value: "1".into(),
            alt_copy_value: None,
            swatch: None,
            preview: None,
        }],
    });
    app.select_1_query = Some(app.search_query.clone());
    app.select_1_ready = true;
    app.copier = Some(vec!["true".into()]);

    let _ = app.update(Message::WindowResized(iced::Size::new(800.0, 600.0)));

    assert!(app.in_flight.is_pending("inline-copy", ""));
}

#[test]
fn test_currency_help_row_is_not_activatable() {
    let mut app = test_app(AddonsConfig::default());
    app.currency.help = true;

    let task = app.update(Message::Submit);

    assert_eq!(task.units(), 0);
}

#[test]
fn test_windows_ctrl_enter_brings_window_here() {
    let mut app = windows_app();
    let _ = app.update(Message::SearchChanged("win notes".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes without confirmation");
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Hypr(HyprAction::MoveHere { address, workspace }))
            if address == "0xb2" && workspace == "2"
    ));
}

#[test]
fn test_windows_ctrl_d_closes_the_window_and_stays_open() {
    let mut app = windows_app();
    let _ = app.update(Message::SearchChanged("win notes".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    let task = app.update(Message::DeleteSelected);
    assert!(task.units() > 0, "the close runs in the background");
    assert!(deferred_action(&app).is_none(), "Raffi stays open");
    assert!(app.in_flight.is_pending("windows", "0xb2"));
    let item = app.command_list.item(0).unwrap().clone();
    assert_eq!(app.command_list_progress(&item), Some("Closing..."));
    assert!(matches!(
        &item.close.as_ref().unwrap().action,
        DeferredAction::Hypr(HyprAction::Close { address }) if address == "0xb2"
    ));

    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Windows,
        app.command_list.session,
        "0xb2".into(),
        Ok(()),
    ));
    assert!(task.units() > 0, "the list reloads");
    assert!(app.command_list.closing.is_none());
    assert_ne!(app.command_list.generation, generation);
}

#[test]
fn test_windows_ctrl_tab_switches_to_workspaces() {
    let mut app = windows_app();
    app.main_view.names = vec![Some("windows".into())];
    app.main_view.window_session = Some("session".into());
    let _ = app.update(Message::SearchChanged("win ".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Windows"));

    let task = app.update(Message::CycleView(false));
    assert!(task.units() > 0, "the workspaces load");
    assert_eq!(app.cycle_view_label().as_deref(), Some("Workspaces"));
    assert_ne!(app.command_list.generation, generation);
    let workspaces = super::windows::hyprland_workspace_items(
        br#"[{"id": 3, "name": "3", "monitor": "DP-2"}, {"id": 1, "name": "1", "monitor": "DP-2"}]"#,
        br#"[{"name": "DP-2", "focused": true, "activeWorkspace": {"id": 1, "name": "1"}}]"#,
        b"[]",
        1,
    )
    .unwrap();
    let _ = app.update(Message::CommandListLoaded(
        app.command_list.generation,
        Ok(workspaces),
    ));
    assert_eq!(
        command_list_titles(&app),
        vec!["Workspace 1", "Workspace 3"]
    );

    let _ = app.update(Message::CycleView(true));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Windows"));
    let _ = app.update(Message::CycleView(false));
    let _ = app.update(Message::CommandListLoaded(
        app.command_list.generation,
        Ok(super::windows::hyprland_workspace_items(
            br#"[{"id": 3, "name": "3", "monitor": "DP-2"}]"#,
            b"[]",
            b"[]",
            1,
        )
        .unwrap()),
    ));
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Hypr(HyprAction::Workspace { workspace })) if workspace == "3"
    ));
    assert!(
        app.main_view.recent.is_empty(),
        "workspaces are not recent window picks"
    );

    // Entering the switcher again lists windows.
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.command_list.windows_view, super::windows::View::Windows);
}

#[test]
fn test_windows_reload_on_compositor_changes_keeps_the_selection() {
    let mut app = windows_app();
    let _ = app.update(Message::SearchChanged("win ".into()));
    let task = app.update(Message::WindowsChanged);
    assert_eq!(task.units(), 0, "the first load is still running");

    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(window_fixture())));
    let _ = app.update(Message::MoveDown);
    assert_eq!(
        command_list_titles(&app)[app.selected_index],
        "Mozilla Firefox"
    );

    let task = app.update(Message::WindowsChanged);
    assert!(task.units() > 0, "the list reloads");
    assert!(app.command_list.refreshing);
    assert_eq!(app.total_items(), 2, "the rows stay while reloading");
    let mut reloaded = window_fixture();
    reloaded.reverse();
    let _ = app.update(Message::CommandListLoaded(
        app.command_list.generation,
        Ok(reloaded),
    ));
    assert_eq!(
        command_list_titles(&app)[app.selected_index],
        "Mozilla Firefox"
    );

    // Leaving the switcher ignores late changes.
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.update(Message::WindowsChanged).units(), 0);
}

#[test]
fn test_windows_status_rows_are_not_actionable() {
    let mut app = windows_app();
    app.compositor = || None;
    let task = app.update(Message::SearchChanged("win ".into()));
    assert!(task.units() > 0, "the list scrolls to the status row");
    assert_eq!(
        app.command_list.status(),
        Some("Window switching needs Hyprland, niri or sway")
    );
    assert_eq!(app.total_items(), 1);
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());

    let mut app = windows_app();
    app.program_exists = |program| program != "hyprctl";
    let _ = app.update(Message::SearchChanged("win ".into()));
    assert_eq!(app.command_list.status(), Some("hyprctl not found"));

    let mut app = windows_app();
    app.compositor = || Some(Compositor::Niri);
    app.program_exists = |program| program != "niri";
    let _ = app.update(Message::SearchChanged("win ".into()));
    assert_eq!(app.command_list.status(), Some("niri not found"));

    let mut app = windows_app();
    app.compositor = || Some(Compositor::Sway);
    app.program_exists = |program| program != "swaymsg";
    let _ = app.update(Message::SearchChanged("win ".into()));
    assert_eq!(app.command_list.status(), Some("swaymsg not found"));

    let mut app = windows_app();
    let _ = app.update(Message::SearchChanged("win ".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Err("hyprctl: timed out after 2s".into()),
    ));
    assert_eq!(
        app.command_list.status(),
        Some("hyprctl: timed out after 2s")
    );
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());
}

#[test]
fn test_windows_stale_load_is_dropped() {
    let mut app = windows_app();
    let _ = app.update(Message::SearchChanged("win ".into()));
    let stale = app.command_list.generation;

    // Leaving the mode drops the pending load.
    let _ = app.update(Message::SearchChanged("firefox".into()));
    let _ = app.update(Message::CommandListLoaded(stale, Ok(window_fixture())));
    assert!(!app.command_list.is_active());
    assert!(app.command_list.items.is_empty());

    // Coming back starts a new load; the old result still does not apply.
    let _ = app.update(Message::SearchChanged("win ".into()));
    assert_ne!(app.command_list.generation, stale);
    let _ = app.update(Message::CommandListLoaded(stale, Ok(window_fixture())));
    assert!(app.command_list.loading);
    assert!(app.command_list.items.is_empty());

    // A list from another addon does not take the result either.
    let current = app.command_list.generation;
    let _ = app.update(Message::SearchChanged("sys ".into()));
    let _ = app.update(Message::CommandListLoaded(current, Ok(window_fixture())));
    assert_eq!(command_list_titles(&app)[0], "Lock");
}

const MEDIA_METADATA: &str =
    "\u{1e}spotify\u{1f}spotify\u{1f}Playing\u{1f}Song\u{1f}Band\u{1f}Album\n\
    \u{1e}mpv\u{1f}mpv\u{1f}Paused\u{1f}clip.mkv\u{1f}\u{1f}\n";

fn media_fixture(keep_open: bool) -> Vec<super::command_list::CommandItem> {
    super::media::media_items(&super::media::parse_metadata(MEDIA_METADATA), keep_open)
}

fn media_app(query: &str) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the players load");
    assert_eq!(app.command_list.status(), Some("Loading players..."));
    let generation = app.command_list.generation;
    let keep_open = !app.addons.media.close_on_action(app.close_on_action);
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(media_fixture(keep_open)),
    ));
    app
}

fn selected_identity(app: &LauncherApp) -> String {
    app.command_list
        .item(app.selected_index)
        .unwrap()
        .identity
        .clone()
}

#[test]
fn test_route_query_media_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("media next", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Media,
            query: "next".into()
        }
    );
    assert!(classify_query("media next", &addons).sensitive);
    assert_eq!(loggable_query("media next", &addons), "<redacted>");
    assert_eq!(
        keyword_suggestions("media", &addons)[0].subtitle,
        "Play, pause or skip in media players"
    );

    addons.media.enabled = false;
    assert_eq!(route_query("media next", &addons), QueryMode::Standard);
}

#[test]
fn test_media_filter_matches_actions_and_players() {
    let mut app = media_app("media ");
    assert_eq!(
        command_list_titles(&app),
        vec![
            "Song · Band",
            "Pause",
            "Next",
            "Previous",
            "Stop",
            "clip.mkv",
            "Play",
            "Next",
            "Previous",
            "Stop"
        ]
    );
    // The header can't be activated.
    let _ = app.update(Message::Submit);
    assert!(!app.in_flight.is_pending("media", "spotify"));
    assert!(deferred_action(&app).is_none());

    let _ = app.update(Message::SearchChanged("media mpv next".into()));
    assert_eq!(command_list_titles(&app)[0], "Next");
    assert_eq!(selected_identity(&app), "mpv/next");

    let _ = app.update(Message::SearchChanged("media spotify".into()));
    assert!(command_list_titles(&app).contains(&"Song · Band"));
    assert!(!command_list_titles(&app).contains(&"clip.mkv"));
}

#[test]
fn test_media_action_keeps_raffi_open_and_refreshes() {
    let mut app = media_app("media mpv next");
    assert_eq!(selected_identity(&app), "mpv/next");

    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "the action runs in the background");
    assert!(deferred_action(&app).is_none(), "Raffi stays open");
    assert!(app.in_flight.is_pending("media", "mpv/next"));
    // Pressing Enter again while it runs does not stack another run.
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0);

    let generation = app.command_list.generation;
    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Media,
        app.command_list.session,
        "mpv/next".into(),
        Ok(()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert!(!app.in_flight.is_pending("media", "mpv/next"));
    assert_ne!(app.command_list.generation, generation);
    assert!(
        app.command_list.status().is_none(),
        "rows stay while refreshing"
    );
    assert_eq!(selected_identity(&app), "mpv/next");

    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(media_fixture(true)),
    ));
    assert!(!app.command_list.refreshing);
    assert_eq!(selected_identity(&app), "mpv/next");
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Next can be pressed again");

    let before = app.history.items.clone();
    app.history.max_items = 10;
    app.save_query_to_history();
    assert_eq!(app.history.items, before);
}

#[test]
fn test_media_action_failure_shows_an_error_row() {
    let mut app = media_app("media mpv next");
    let _ = app.update(Message::Submit);
    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Media,
        app.command_list.session,
        "mpv/next".into(),
        Err("playerctl: exited with exit status: 1".into()),
    ));
    assert_eq!(task.units(), 0, "nothing reloads after a failure");
    assert!(!app.in_flight.is_pending("media", "mpv/next"));
    assert_eq!(
        command_list_titles(&app)[0],
        "Next failed: playerctl: exited with exit status: 1"
    );
    assert_eq!(selected_identity(&app), "mpv/next");

    // The error row itself does nothing.
    app.selected_index = 0;
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0);
}

#[test]
fn test_media_action_finishing_after_leaving_changes_nothing() {
    let mut app = media_app("media mpv next");
    let session = app.command_list.session;
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::SearchChanged("sys ".into()));
    let generation = app.command_list.generation;
    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Media,
        session,
        "mpv/next".into(),
        Err("boom".into()),
    ));
    assert_eq!(task.units(), 0);
    assert!(!app.in_flight.is_pending("media", "mpv/next"));
    assert_eq!(app.command_list.addon, Some(CommandListAddon::System));
    assert_eq!(app.command_list.generation, generation);
    assert_eq!(command_list_titles(&app)[0], "Lock");

    // Reopening the controls before it finishes does not let it in either.
    let mut app = media_app("media mpv next");
    let session = app.command_list.session;
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::SearchChanged("firefox".into()));
    let mut reopened = media_app_reopen(app);
    let generation = reopened.command_list.generation;
    for result in [Ok(()), Err("boom".to_string())] {
        let task = reopened.update(Message::CommandListActionDone(
            CommandListAddon::Media,
            session,
            "mpv/next".into(),
            result,
        ));
        assert_eq!(task.units(), 0);
    }
    assert!(!reopened.in_flight.is_pending("media", "mpv/next"));
    assert_eq!(reopened.command_list.generation, generation);
    assert!(!command_list_titles(&reopened).contains(&"Next failed: boom"));
}

fn media_app_reopen(mut app: LauncherApp) -> LauncherApp {
    let _ = app.update(Message::SearchChanged("media ".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(media_fixture(true)),
    ));
    app
}

#[test]
fn test_media_failure_survives_a_refresh_started_before_it() {
    let mut app = media_app("media mpv");
    let session = app.command_list.session;
    let _ = app.update(Message::CommandListActionDone(
        CommandListAddon::Media,
        session,
        "mpv/next".into(),
        Ok(()),
    ));
    let refresh = app.command_list.generation;
    let _ = app.update(Message::CommandListActionDone(
        CommandListAddon::Media,
        session,
        "mpv/stop".into(),
        Err("boom".into()),
    ));
    assert_eq!(command_list_titles(&app)[0], "Stop failed: boom");
    let _ = app.update(Message::CommandListLoaded(refresh, Ok(media_fixture(true))));
    let titles = command_list_titles(&app);
    assert_eq!(titles[0], "Stop failed: boom");
    assert_eq!(titles.iter().filter(|t| t.contains("failed")).count(), 1);

    // A later success clears it once its refresh lands.
    let _ = app.update(Message::CommandListActionDone(
        CommandListAddon::Media,
        session,
        "mpv/next".into(),
        Ok(()),
    ));
    let refresh = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(refresh, Ok(media_fixture(true))));
    assert!(!command_list_titles(&app)[0].contains("failed"));
}

#[test]
fn test_media_close_on_action_runs_after_close() {
    let mut addons = AddonsConfig::default();
    addons.media.close_on_action = Some(true);
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("media spotify pause".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(media_fixture(false)),
    ));
    assert_eq!(selected_identity(&app), "spotify/play-pause");
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes");
    assert_eq!(
        deferred_argv(&app),
        Some(vec![
            "playerctl".to_string(),
            "-p".to_string(),
            "spotify".to_string(),
            "play-pause".to_string()
        ])
    );
}

#[test]
fn test_media_missing_playerctl_and_no_players() {
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |program| program != "playerctl";
    let task = app.update(Message::SearchChanged("media ".into()));
    assert!(task.units() > 0, "the list scrolls to the status row");
    assert_eq!(app.command_list.status(), Some("playerctl not found"));
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());

    let mut app = test_app(AddonsConfig::default());
    let _ = app.update(Message::SearchChanged("media next".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::media::media_items(&[], true)),
    ));
    assert_eq!(command_list_titles(&app), vec!["No media players running"]);
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0);
}

fn audio_app(query: &str) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the devices load");
    assert_eq!(app.command_list.status(), Some("Loading audio devices..."));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::audio::fixture_items(true)),
    ));
    app
}

#[test]
fn test_route_query_audio_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("audio razer", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Audio,
            query: "razer".into()
        }
    );
    assert!(classify_query("audio razer", &addons).sensitive);
    assert_eq!(loggable_query("audio razer", &addons), "<redacted>");
    assert_eq!(
        keyword_suggestions("audio", &addons)[0].subtitle,
        "Switch or mute audio outputs and inputs"
    );

    addons.audio.enabled = false;
    assert_eq!(route_query("audio razer", &addons), QueryMode::Standard);
}

#[test]
fn test_audio_ctrl_tab_cycles_outputs_and_inputs() {
    let mut app = audio_app("audio ");
    assert_eq!(app.cycle_view_label().as_deref(), Some("All"));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Outputs"));
    assert_eq!(
        command_list_titles(&app),
        vec![
            "Volume",
            "Volume up",
            "Volume down",
            "Unmute output",
            "Outputs",
            "Ryzen HD Audio Controller Pro",
            "✓ Razer Seiren V2 Pro Analog Stereo",
            "WH-1000XM4",
        ]
    );
    let _ = app.update(Message::CycleView(false));
    assert_eq!(
        command_list_titles(&app),
        vec![
            "Volume",
            "Mute microphone",
            "Inputs",
            "Ryzen HD Audio Controller Pro",
            "✓ Razer Seiren V2 Pro Mono",
        ]
    );
    // The view stays while typing; a volume only sets outputs.
    let _ = app.update(Message::SearchChanged("audio 40".into()));
    assert!(!command_list_titles(&app)
        .iter()
        .any(|title| title.starts_with("Set output volume")));
    let _ = app.update(Message::CycleView(true));
    assert_eq!(app.command_list.audio_view, super::audio::View::Outputs);
    assert_eq!(command_list_titles(&app)[0], "Set output volume to 40%");

    // Entering the list again shows every device.
    let _ = app.update(Message::SearchChanged(String::new()));
    let app = audio_app("audio ");
    assert_eq!(app.command_list.audio_view, super::audio::View::All);
}

#[test]
fn test_audio_rows_and_filtering() {
    let mut app = audio_app("audio ");
    assert_eq!(
        command_list_titles(&app),
        vec![
            "Volume",
            "Volume up",
            "Volume down",
            "Unmute output",
            "Mute microphone",
            "Outputs",
            "Ryzen HD Audio Controller Pro",
            "✓ Razer Seiren V2 Pro Analog Stereo",
            "WH-1000XM4",
            "Inputs",
            "Ryzen HD Audio Controller Pro",
            "✓ Razer Seiren V2 Pro Mono",
        ]
    );
    // Headers can't be activated.
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0);
    assert!(deferred_action(&app).is_none());

    let _ = app.update(Message::SearchChanged("audio headphones".into()));
    assert_eq!(command_list_titles(&app), vec!["WH-1000XM4"]);
    let _ = app.update(Message::SearchChanged("audio microphone razer".into()));
    assert_eq!(
        selected_identity(&app),
        format!("source/{}", super::audio::DEFAULT_SOURCE)
    );
    for (query, header, kind) in [
        ("audio inputs", "Inputs", "source/"),
        ("audio outputs", "Outputs", "sink/"),
    ] {
        let _ = app.update(Message::SearchChanged(query.into()));
        assert_eq!(command_list_titles(&app)[0], header);
        let count = if kind == "sink/" { 3 } else { 2 };
        let listed = app
            .command_list
            .filtered
            .iter()
            .filter(|&&index| app.command_list.items[index].identity.starts_with(kind))
            .count();
        assert_eq!(listed, count, "{query}");
    }

    // A typed volume comes first and sets the default output.
    let _ = app.update(Message::SearchChanged("audio 40".into()));
    assert_eq!(command_list_titles(&app)[0], "Set output volume to 40%");
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "the volume is set in the background");
    assert!(app.in_flight.is_pending("audio", super::audio::SET_VOLUME));
    let _ = app.update(Message::SearchChanged("audio razer".into()));
    assert!(!command_list_titles(&app)
        .iter()
        .any(|title| title.starts_with("Set output volume")));
}

#[test]
fn test_audio_actions_keep_raffi_open_and_refresh_on_the_same_device() {
    let mut app = audio_app("audio ");
    app.selected_index = 8;
    let identity = "sink/bluez_output.00_11_22_33_44_55.1".to_string();
    assert_eq!(selected_identity(&app), identity);

    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "the action runs in the background");
    assert!(deferred_action(&app).is_none(), "Raffi stays open");
    assert!(app.in_flight.is_pending("audio", &identity));
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0, "no second run while one is pending");

    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Audio,
        app.command_list.session,
        identity.clone(),
        Ok(()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert!(!app.in_flight.is_pending("audio", &identity));
    assert!(app.command_list.status().is_none());

    // The refreshed list moved the device; the selection follows it.
    let mut items = super::audio::fixture_items(true);
    let moved = items.remove(8);
    items.insert(1, moved);
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(items)));
    assert_eq!(app.selected_index, 1);
    assert_eq!(selected_identity(&app), identity);

    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let task = app.update(Message::Submit);
    assert!(
        task.units() > 0,
        "Ctrl+Enter toggles mute in the background"
    );
    assert!(deferred_action(&app).is_none());
}

#[test]
fn test_audio_action_failure_shows_an_error_row() {
    let mut app = audio_app("audio wh-1000");
    let identity = "sink/bluez_output.00_11_22_33_44_55.1".to_string();
    let _ = app.update(Message::Submit);
    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Audio,
        app.command_list.session,
        identity.clone(),
        Err("pactl: exited with exit status: 1".into()),
    ));
    assert_eq!(task.units(), 0);
    assert_eq!(
        command_list_titles(&app)[0],
        "WH-1000XM4 failed: pactl: exited with exit status: 1"
    );
    assert_eq!(selected_identity(&app), identity);
}

#[test]
fn test_audio_close_on_action_runs_after_close() {
    let mut addons = AddonsConfig::default();
    addons.audio.close_on_action = Some(true);
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("audio wh-1000".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::audio::fixture_items(false)),
    ));
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes");
    assert_eq!(
        deferred_argv(&app),
        Some(vec![
            "pactl".to_string(),
            "set-sink-mute".to_string(),
            "bluez_output.00_11_22_33_44_55.1".to_string(),
            "toggle".to_string()
        ])
    );
}

#[test]
fn test_audio_missing_pactl() {
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |program| program != "pactl";
    let task = app.update(Message::SearchChanged("audio ".into()));
    assert!(task.units() > 0, "the list scrolls to the status row");
    assert_eq!(app.command_list.status(), Some("pactl not found"));
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());
}

fn terminal_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.terminal.command = Some(vec!["foot".into()]);
    addons.terminal.hold = false;
    addons
}

#[test]
fn test_terminal_runs_the_query_and_keeps_it_in_history() {
    let mut app = test_app(terminal_addons());
    app.history.max_items = 0;
    assert_eq!(
        keyword_suggestions(">", &app.addons)[0].subtitle,
        "Run a command in a terminal"
    );

    let _ = app.update(Message::SearchChanged("> ".into()));
    assert_eq!(
        command_list_titles(&app),
        vec!["Type a command to run in foot"]
    );
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none(), "the hint does nothing");

    let _ = app.update(Message::SearchChanged("> htop -d 5".into()));
    assert_eq!(command_list_titles(&app), vec!["Run htop -d 5"]);
    assert!(app.query_class.saved_in_history());
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "Raffi closes");
    assert_eq!(
        deferred_argv(&app),
        Some(
            ["foot", "bash", "-c", "eval \"$1\"", "raffi", "htop -d 5"]
                .map(String::from)
                .to_vec()
        )
    );
}

#[test]
fn test_terminal_without_a_terminal_says_how_to_set_one() {
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |_| false;
    let _ = app.update(Message::SearchChanged("> htop".into()));
    assert_eq!(
        app.command_list.status(),
        Some("No terminal found, set addons.terminal.command")
    );
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());
}

#[test]
fn test_terminal_fallback_runs_the_query() {
    let mut app = test_app_with_fallbacks(terminal_addons(), vec!["addons.terminal".into()]);
    app.history.max_items = 0;
    assert_eq!(app.fallbacks.len(), 1);
    assert_eq!(
        app.fallbacks[0].label("make test"),
        "Run 'make test' in foot"
    );

    let _ = app.update(Message::SearchChanged("make test".into()));
    assert_eq!(app.fallback_count(), 1);
    let _ = app.update(Message::FallbackSelected(0));
    assert_eq!(
        deferred_argv(&app).map(|argv| argv.last().cloned()),
        Some(Some("make test".to_string()))
    );
}

fn terminal_row(app: &LauncherApp) -> &super::command_list::CommandItem {
    &app.command_list.items[app.command_list.filtered[0]]
}

fn kitty_tab(_: &[String], socket: Option<&str>) -> Option<crate::terminal::TabTarget> {
    Some(crate::terminal::TabTarget {
        kind: crate::terminal::TabKind::Kitty,
        socket: socket.unwrap_or("unix:/tmp/k-7").to_string(),
        pid: Some(7),
    })
}

fn kitty_addons() -> AddonsConfig {
    let mut addons = terminal_addons();
    addons.terminal.command = Some(vec!["kitty".into()]);
    addons
}

#[test]
fn test_terminal_opens_a_tab_in_the_running_kitty() {
    let mut app = test_app(kitty_addons());
    app.terminal_tab = kitty_tab;
    let _ = app.update(Message::SearchChanged("> htop".into()));
    assert_eq!(terminal_row(&app).subtitle, "in a new kitty tab");
    assert!(terminal_row(&app)
        .details
        .contains(&("Opens in".to_string(), "New kitty tab".to_string())));
    let _ = app.update(Message::Submit);
    let Some(DeferredAction::TerminalTab {
        tab, window, raise, ..
    }) = app.deferred_action.lock().unwrap().clone()
    else {
        panic!("terminal tab action");
    };
    assert_eq!(tab[..4], ["kitty", "@", "--to", "unix:/tmp/k-7"]);
    assert_eq!(tab.last().map(String::as_str), Some("htop"));
    assert_eq!(window[0], "kitty", "a window when the tab fails");
    assert_eq!(raise, Some((7, "htop".to_string())));

    let mut app = test_app_with_fallbacks(kitty_addons(), vec!["addons.terminal".into()]);
    app.history.max_items = 0;
    app.terminal_tab = kitty_tab;
    let _ = app.update(Message::SearchChanged("make test".into()));
    let _ = app.update(Message::FallbackSelected(0));
    assert!(matches!(
        app.deferred_action.lock().unwrap().as_ref(),
        Some(DeferredAction::TerminalTab { .. })
    ));
}

#[test]
fn test_terminal_open_in_window_ignores_a_running_kitty() {
    let mut addons = kitty_addons();
    addons.terminal.open_in = crate::TerminalOpenIn::Window;
    let mut app = test_app(addons);
    app.terminal_tab = kitty_tab;
    let _ = app.update(Message::SearchChanged("> htop".into()));
    assert_eq!(terminal_row(&app).subtitle, "in kitty");
    let _ = app.update(Message::Submit);
    assert_eq!(deferred_argv(&app).unwrap()[0], "kitty");
}

fn launcher_names(app: &LauncherApp) -> Vec<String> {
    app.filtered_configs
        .iter()
        .map(|&index| app.configs[index].description.clone().unwrap_or_default())
        .collect()
}

fn devtools_app() -> LauncherApp {
    let configs = vec![
        RaffiConfig {
            binary: Some("pwvucontrol".into()),
            description: Some("pwvucontrol".into()),
            ..Default::default()
        },
        RaffiConfig {
            binary: Some("firefox".into()),
            description: Some("Firefox".into()),
            ..Default::default()
        },
    ];
    let (mut app, _) = LauncherApp::new(
        configs,
        AddonsConfig::default(),
        true,
        Arc::new(Mutex::new(None)),
        None,
        ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None),
        10,
        FontSizes::default_sizes(),
        SortMode::Hybrid,
        Vec::new(),
    );
    app.mru_map.clear();
    app.text_snippets.mru_map.clear();
    app.text_snippets.mru_path = None;
    app.program_exists = |_| true;
    app.history.max_items = 10;
    app.history.items = vec!["older".into()];
    app
}

#[test]
fn test_generator_queries_classify_as_sensitive() {
    let addons = AddonsConfig::default();
    for (query, provider) in [
        ("uuid", Some(InlineProvider::Uuid)),
        ("pw", Some(InlineProvider::Password)),
        ("pw 32", Some(InlineProvider::Password)),
        ("uuidgen", None),
        ("pwvu", None),
        ("pw x", None),
    ] {
        let class = classify_query(query, &addons);
        assert_eq!(class.inline_provider, provider, "{query}");
        assert_eq!(class.sensitive, provider.is_some(), "{query}");
    }
    assert_eq!(loggable_query("pw 20", &addons), "<redacted>");
    assert_eq!(loggable_query("uuid", &addons), "<redacted>");
    // Math still wins over the generators.
    assert_eq!(
        classify_query("2*3", &addons).inline_provider,
        Some(InlineProvider::Calculator)
    );

    let mut disabled = addons;
    disabled.devtools.enabled = false;
    assert_eq!(classify_query("pw", &disabled).inline_provider, None);
    assert!(!classify_query("pw", &disabled).sensitive);
}

#[test]
fn test_password_row_keeps_launchers_and_stays_out_of_history() {
    let mut app = devtools_app();
    let _ = app.update(Message::SearchChanged("pw".into()));
    let inline = app.inline_result.as_ref().expect("password row");
    assert_eq!(inline.provider, InlineProvider::Password);
    let password = inline.rows[0].copy_value.clone();
    assert_eq!(password.chars().count(), 20);
    assert!(password.chars().all(super::devtools::is_password_char));
    assert_eq!(inline.rows[0].title, password);
    // Launchers starting with "pw" are still listed below the row.
    assert_eq!(launcher_names(&app), ["pwvucontrol"]);
    assert_eq!(app.total_items(), 2);

    // The value is generated once per query change, not per lookup.
    assert!(app.resolve_selected_preview().is_some());
    assert_eq!(app.inline_copy_value(0).as_deref(), Some(password.as_str()));

    let _ = app.update(Message::SearchChanged("pw 32".into()));
    let longer = &app.inline_result.as_ref().unwrap().rows[0].copy_value;
    assert_eq!(longer.chars().count(), 32);
    assert_ne!(longer, &password);

    // Launching the app under the generator row saves nothing either.
    let _ = app.update(Message::SearchChanged("pw".into()));
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::Submit);
    assert_eq!(
        app.selected_item.lock().unwrap().as_deref(),
        Some("pwvucontrol")
    );
    assert_eq!(app.history.items, ["older"]);
}

#[test]
fn test_uuid_row_copies_through_checked_copy() {
    let mut app = devtools_app();
    app.copier = failing_copier();
    let _ = app.update(Message::SearchChanged("uuid".into()));
    let uuid = app.inline_result.as_ref().unwrap().rows[0]
        .copy_value
        .clone();
    assert!(super::devtools::is_uuid_v4(&uuid), "{uuid}");

    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    let result = app.copy_job(uuid)();
    let task = app.update(Message::InlineCopied(app.inline_generation, result));
    assert_eq!(task.units(), 0, "a failed copy keeps Raffi open");
    assert!(app.inline_copy_error.is_some());

    let task = app.update(Message::InlineCopied(app.inline_generation, Ok(())));
    assert!(task.units() > 0);
    assert_eq!(app.history.items, ["older"]);
}

fn fake_hash_command(script: &str) -> Option<Vec<String>> {
    Some(vec!["sh".into(), "-c".into(), script.into()])
}

#[test]
fn test_devtools_route_keeps_raw_payload_and_is_sensitive() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("dev  a b ", &addons),
        QueryMode::Devtools {
            payload: " a b ".into()
        }
    );
    assert_eq!(
        route_query("  dev x", &addons),
        QueryMode::Devtools {
            payload: "x".into()
        }
    );
    assert_eq!(route_query("dev", &addons), QueryMode::Standard);
    assert_eq!(route_query("devhelp", &addons), QueryMode::Standard);

    let class = classify_query("dev my secret", &addons);
    assert!(class.sensitive);
    assert_eq!(class.inline_provider, None);
    assert_eq!(class.loggable_query("dev my secret", &addons), "<redacted>");
    assert!(!format!("{class:?}").contains("secret"));
    assert!(keyword_suggestions("dev", &addons)
        .iter()
        .any(|suggestion| suggestion.keyword == "dev"));

    // A user web search with the same keyword wins.
    addons.web_searches.push(WebSearchConfig {
        name: "Dev docs".into(),
        keyword: "dev".into(),
        url: "https://devdocs.io/#q={query}".into(),
        icon: None,
        ..Default::default()
    });
    assert!(matches!(
        route_query("dev x", &addons),
        QueryMode::WebSearch { .. }
    ));
    addons.web_searches.clear();
    addons.devtools.trigger = Some("tx".into());
    assert_eq!(
        route_query("tx  ", &addons),
        QueryMode::Devtools {
            payload: " ".into()
        }
    );
}

#[test]
fn test_devtools_rows_hash_asynchronously_and_drop_stale_results() {
    let mut app = devtools_app();
    app.devtools.hash_command = fake_hash_command("cat >/dev/null; echo 'ABCDEF01  -'");
    let _ = app.update(Message::SearchChanged("dev hi".into()));
    assert!(app.devtools.active);
    assert!(app.filtered_configs.is_empty());
    assert!(app.inline_result.is_none());
    let titles: Vec<&str> = app.devtools.rows.iter().map(|row| row.title).collect();
    assert_eq!(
        titles,
        ["Base64 encode", "Uppercase", "SHA-256", "SHA-1", "MD5"]
    );
    assert_eq!(app.devtools.rows[0].value, "aGk=");
    assert_eq!(app.total_items(), app.devtools.rows.len());
    assert!(app.resolve_selected_preview().is_some());

    // Pending hash rows can't be activated.
    let sha_index = 2;
    assert_eq!(
        app.devtools.rows[sha_index].value,
        super::devtools::HASH_PENDING
    );
    app.selected_index = sha_index;
    let _ = app.update(Message::Submit);
    assert!(!app.in_flight.is_pending("inline-copy", ""));

    let jobs: Vec<_> = app
        .devtools_hash_jobs()
        .into_iter()
        .map(|(title, argv)| (title, argv.unwrap()))
        .collect();
    let hashed = super::devtools::compute_hash_rows(&jobs, "hi");
    let stale = app.devtools.current_generation().wrapping_sub(1);
    let _ = app.update(Message::DevtoolsHashes(stale, hashed.clone()));
    assert_eq!(
        app.devtools.rows[sha_index].value,
        super::devtools::HASH_PENDING
    );

    let started = app.devtools.current_generation();
    let _ = app.update(Message::DevtoolsHashes(started, hashed.clone()));
    assert_eq!(app.devtools.rows[sha_index].value, "abcdef01");
    assert!(app.devtools.rows[sha_index].copyable);

    // Results computed for an older query are dropped.
    let _ = app.update(Message::SearchChanged("dev hj".into()));
    let _ = app.update(Message::DevtoolsHashes(started, hashed));
    assert!(app.devtools.rows[sha_index..]
        .iter()
        .all(|row| row.value == super::devtools::HASH_PENDING));

    // Enter copies the selected result; the query never reaches history.
    let _ = app.update(Message::Submit);
    assert!(app.in_flight.is_pending("inline-copy", ""));
    let task = app.update(Message::InlineCopied(app.inline_generation, Ok(())));
    assert!(task.units() > 0);
    assert_eq!(app.history.items, ["older"]);

    // Leaving the mode clears its rows.
    let _ = app.update(Message::SearchChanged("fire".into()));
    assert!(!app.devtools.active);
    assert!(app.devtools.rows.is_empty());
    assert_eq!(launcher_names(&app), ["Firefox"]);
}

#[test]
fn test_devtools_hash_errors_are_rows_without_the_input() {
    let mut app = devtools_app();
    app.program_exists = |program| program != "md5sum";
    app.devtools.hash_command = fake_hash_command("cat >&2; exit 2");
    let _ = app.update(Message::SearchChanged("dev topsecret".into()));
    let md5 = app.devtools.rows.last().unwrap();
    assert_eq!((md5.title, md5.value.as_str()), ("MD5", "md5sum not found"));
    assert!(!md5.copyable);

    let jobs: Vec<_> = app
        .devtools_hash_jobs()
        .into_iter()
        .filter_map(|(title, argv)| argv.ok().map(|argv| (title, argv)))
        .collect();
    assert_eq!(jobs.len(), 2);
    let rows = super::devtools::compute_hash_rows(&jobs, "topsecret");
    let _ = app.update(Message::DevtoolsHashes(
        app.devtools.current_generation(),
        rows,
    ));
    let sha = app
        .devtools
        .rows
        .iter()
        .find(|row| row.title == "SHA-256")
        .unwrap();
    assert_eq!(sha.value, "Hash failed: sh failed (exit 2)");
    assert!(!sha.copyable);
    assert!(app
        .devtools
        .rows
        .iter()
        .filter(|row| !row.copyable)
        .all(|row| !row.value.contains("topsecret")));

    let index = app.devtools.rows.len() - 1;
    app.selected_index = index;
    let _ = app.update(Message::Submit);
    assert!(!app.in_flight.is_pending("inline-copy", ""));
}

fn kill_app(query: &str) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the processes load");
    assert_eq!(app.command_list.status(), Some("Loading processes..."));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::kill::fixture_items()),
    ));
    app
}

#[test]
fn test_route_query_kill_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("kill firefox", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Kill,
            query: "firefox".into()
        }
    );
    assert!(classify_query("kill firefox", &addons).sensitive);
    assert_eq!(loggable_query("kill firefox", &addons), "<redacted>");
    assert_eq!(
        keyword_suggestions("kill", &addons)[0].subtitle,
        "Stop one of your processes"
    );

    addons.kill.enabled = false;
    assert_eq!(route_query("kill firefox", &addons), QueryMode::Standard);
}

#[test]
fn test_kill_rows_sorted_by_memory_and_filtered() {
    let mut app = kill_app("kill ");
    assert_eq!(
        command_list_titles(&app),
        vec!["firefox", "bash", "a) b (c"]
    );
    let _ = app.update(Message::SearchChanged("kill odd tool".into()));
    assert_eq!(command_list_titles(&app), vec!["a) b (c"]);
    let _ = app.update(Message::SearchChanged("kill 202".into()));
    assert_eq!(selected_identity(&app), "202/7000");
}

#[test]
fn test_kill_needs_confirmation_bound_to_process_and_signal() {
    let mut app = kill_app("kill firefox");
    let identity = "201/6000".to_string();

    // A first Enter only arms SIGTERM; Ctrl+Enter then arms SIGKILL instead.
    assert_eq!(app.update(Message::Submit).units(), 0);
    assert!(app.command_list.armed.is_some());
    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    assert!(app.command_list.armed.is_none());
    assert_eq!(app.update(Message::Submit).units(), 0);
    assert_eq!(
        app.command_list.armed.as_ref().map(|armed| armed.modifiers),
        Some(iced::keyboard::Modifiers::CTRL)
    );
    assert_eq!(app.update(Message::SubmitRepeat).units(), 0);
    assert!(!app.in_flight.is_pending("kill", &identity));

    // A refresh replacing the list clears the confirmation.
    app.command_list.begin_refresh();
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::kill::fixture_items()),
    ));
    assert!(app.command_list.armed.is_none());

    assert_eq!(app.update(Message::Submit).units(), 0);
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "the signal is sent in the background");
    assert!(deferred_action(&app).is_none(), "Raffi stays open");
    assert!(app.in_flight.is_pending("kill", &identity));
    assert_eq!(app.update(Message::Submit).units(), 0);
    assert_eq!(app.update(Message::Submit).units(), 0);

    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Kill,
        app.command_list.session,
        identity.clone(),
        Ok(()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert!(!app.in_flight.is_pending("kill", &identity));
}

#[test]
fn test_kill_failure_shows_an_error_row_and_refreshes() {
    let mut app = kill_app("kill ");
    let identity = "201/6000".to_string();
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::Submit);
    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Kill,
        app.command_list.session,
        identity,
        Err("process 201 has exited".into()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert_eq!(
        command_list_titles(&app)[0],
        "firefox failed: process 201 has exited"
    );

    // The refresh no longer lists the process; the error stays on top.
    let mut items = super::kill::fixture_items();
    items.remove(0);
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(items)));
    assert_eq!(
        command_list_titles(&app),
        vec!["firefox failed: process 201 has exited", "bash", "a) b (c"]
    );
    assert_eq!(app.update(Message::ItemClicked(0)).units(), 0);
}

#[test]
fn test_kill_missing_program_and_history() {
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |program| program != "kill";
    let task = app.update(Message::SearchChanged("kill ".into()));
    assert!(task.units() > 0, "the list scrolls to the status row");
    assert_eq!(app.command_list.status(), Some("kill not found"));

    let mut app = kill_app("kill firefox");
    app.history.max_items = 10;
    let before = app.history.items.clone();
    app.save_query_to_history();
    assert_eq!(app.history.items, before);
}

fn bluetooth_app(query: &str, powered: bool) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the devices load");
    assert_eq!(
        app.command_list.status(),
        Some("Loading Bluetooth devices...")
    );
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::bluetooth::fixture_items(powered)),
    ));
    app
}

fn select_identity(app: &mut LauncherApp, identity: &str) {
    app.selected_index = app.command_list.position(identity).unwrap();
}

#[test]
fn test_route_query_bluetooth_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("bt buds", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Bluetooth,
            query: "buds".into()
        }
    );
    assert!(classify_query("bt buds", &addons).sensitive);
    assert_eq!(loggable_query("bt buds", &addons), "<redacted>");
    assert_eq!(
        keyword_suggestions("bt", &addons)[0].subtitle,
        "Connect Bluetooth devices or turn Bluetooth on and off"
    );

    addons.bluetooth.enabled = false;
    assert_eq!(route_query("bt buds", &addons), QueryMode::Standard);
}

#[test]
fn test_bluetooth_connect_runs_at_once_and_shows_progress() {
    let mut app = bluetooth_app("bt ", true);
    assert_eq!(
        command_list_titles(&app),
        vec![
            "Bluetooth is on",
            "✓ Studio  Headphones",
            "My Buds",
            "✓ Keyboard K3"
        ]
    );
    let identity = "device/00:11:22:33:44:02".to_string();
    select_identity(&mut app, &identity);
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "connecting runs in the background");
    assert!(deferred_action(&app).is_none(), "Raffi stays open");
    assert!(app.in_flight.is_pending("bluetooth", &identity));
    let row = app.command_list.item(app.selected_index).unwrap().clone();
    assert_eq!(app.command_list_progress(&row), Some("Connecting..."));
    assert_eq!(app.update(Message::Submit).units(), 0, "no second connect");

    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Bluetooth,
        app.command_list.session,
        identity.clone(),
        Ok(()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert!(!app.in_flight.is_pending("bluetooth", &identity));
    assert_eq!(app.command_list_progress(&row), None);
}

#[test]
fn test_bluetooth_selection_stays_on_the_device_after_the_refresh() {
    let mut app = bluetooth_app("bt ", true);
    let identity = "device/00:11:22:33:44:02";
    select_identity(&mut app, identity);
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::CommandListActionDone(
        CommandListAddon::Bluetooth,
        app.command_list.session,
        identity.to_string(),
        Ok(()),
    ));

    let mut items = super::bluetooth::fixture_items(true);
    let connected = super::bluetooth::DeviceInfo {
        name: "Ear Buds".to_string(),
        alias: "My Buds".to_string(),
        icon: "audio-headset".to_string(),
        connected: true,
        paired: true,
        battery: None,
    };
    let position = items
        .iter()
        .position(|item| item.identity == identity)
        .unwrap();
    items[position] =
        super::bluetooth::device_item("00:11:22:33:44:02", "Ear Buds", Some(&connected), true);
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(items)));

    let selected = app.command_list.item(app.selected_index).unwrap();
    assert_eq!(selected.identity, identity);
    assert_eq!(selected.title, "✓ My Buds");
}

#[test]
fn test_bluetooth_disconnect_and_power_off_need_confirmation() {
    for identity in ["device/00:11:22:33:44:01", "adapter/power-off"] {
        let mut app = bluetooth_app("bt ", true);
        select_identity(&mut app, identity);

        assert_eq!(app.update(Message::Submit).units(), 0);
        assert!(app.command_list.armed.is_some());
        assert_eq!(app.update(Message::SubmitRepeat).units(), 0);
        assert!(!app.in_flight.is_pending("bluetooth", identity));

        // Moving away resets the confirmation.
        let _ = app.update(Message::SearchChanged("bt k".into()));
        assert!(app.command_list.armed.is_none());
        let _ = app.update(Message::SearchChanged("bt ".into()));
        select_identity(&mut app, identity);

        // A refresh replacing the list clears it too.
        assert_eq!(app.update(Message::Submit).units(), 0);
        app.command_list.begin_refresh();
        let generation = app.command_list.generation;
        let _ = app.update(Message::CommandListLoaded(
            generation,
            Ok(super::bluetooth::fixture_items(true)),
        ));
        assert!(app.command_list.armed.is_none());

        assert_eq!(app.update(Message::Submit).units(), 0);
        assert!(app.update(Message::Submit).units() > 0);
        assert!(app.in_flight.is_pending("bluetooth", identity));
    }
}

#[test]
fn test_bluetooth_filter_keeps_the_power_row_first() {
    let mut app = bluetooth_app("bt ", true);
    let _ = app.update(Message::SearchChanged("bt buds".into()));
    assert_eq!(
        command_list_titles(&app),
        vec!["Bluetooth is on", "My Buds"]
    );
    let _ = app.update(Message::SearchChanged("bt zzzz".into()));
    assert_eq!(command_list_titles(&app), vec!["Bluetooth is on"]);
}

#[test]
fn test_bluetooth_adapter_off_and_power_on() {
    let mut app = bluetooth_app("bt buds", false);
    assert_eq!(command_list_titles(&app), vec!["Bluetooth is off"]);
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "turning on needs no confirmation");
    assert!(app.in_flight.is_pending("bluetooth", "adapter/power-on"));
}

#[test]
fn test_bluetooth_failure_shows_an_error_row_and_refreshes() {
    let mut app = bluetooth_app("bt ", true);
    let identity = "device/00:11:22:33:44:02".to_string();
    select_identity(&mut app, &identity);
    let _ = app.update(Message::Submit);
    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::Bluetooth,
        app.command_list.session,
        identity,
        Err("Failed to connect: org.bluez.Error.Failed".into()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert_eq!(
        command_list_titles(&app)[0],
        "My Buds failed: Failed to connect: org.bluez.Error.Failed"
    );
    assert_eq!(app.update(Message::ItemClicked(0)).units(), 0);
}

#[test]
fn test_bluetooth_close_on_action_defers_the_action() {
    let mut addons = AddonsConfig::default();
    addons.bluetooth.close_on_action = Some(true);
    let mut app = test_app(addons);
    let _ = app.update(Message::SearchChanged("bt ".into()));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::bluetooth::load_devices_with(
            super::bluetooth::fixture_runner(true),
            std::time::Duration::from_secs(3),
            false,
        )),
    ));
    select_identity(&mut app, "device/00:11:22:33:44:02");
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(super::actions::DeferredAction::Bluetooth(args)) if args == ["connect", "00:11:22:33:44:02"]
    ));
}

#[test]
fn test_bluetooth_missing_program_and_history() {
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |program| program != "bluetoothctl";
    let task = app.update(Message::SearchChanged("bt ".into()));
    assert!(task.units() > 0, "the list scrolls to the status row");
    assert_eq!(app.command_list.status(), Some("bluetoothctl not found"));

    let mut app = bluetooth_app("bt buds", true);
    app.history.max_items = 10;
    let before = app.history.items.clone();
    app.save_query_to_history();
    assert_eq!(app.history.items, before);
}

fn file_preview_app(command: Option<&str>) -> (LauncherApp, String, String, std::path::PathBuf) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("raffi-file-preview-command-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut paths = Vec::new();
    let mut app = test_app(AddonsConfig::default());
    app.file_browser.preview_command =
        command.map(|c| super::file_preview::FilePreviewCommand::Custom(c.into()));
    app.keyword_suggestions.clear();
    app.file_browser.active = true;
    for name in ["first.txt", "second.txt"] {
        let path = dir.join(name);
        std::fs::write(&path, format!("{name} contents")).unwrap();
        let full_path = path.to_string_lossy().into_owned();
        paths.push(full_path.clone());
        app.file_browser
            .entries
            .push(super::state::FileBrowserEntry {
                name: name.into(),
                full_path,
                is_dir: false,
            });
    }
    app.refresh_file_preview_cache();
    let second = paths.pop().unwrap();
    let first = paths.pop().unwrap();
    (app, first, second, dir)
}

#[test]
fn test_file_preview_command_is_idle_when_unset_or_pane_hidden() {
    let (mut app, _, _, dir) = file_preview_app(None);
    let _ = app.ensure_file_browser_preview();
    assert_eq!(app.file_browser.preview_target, None);

    app.file_browser.preview_command = Some(super::file_preview::FilePreviewCommand::Custom(
        "cat".into(),
    ));
    app.view.show_preview = false;
    let _ = app.ensure_file_browser_preview();
    assert_eq!(app.file_browser.preview_target, None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_file_preview_command_follows_selection_one_run_at_a_time() {
    use super::file_preview::{FilePreviewKey, FilePreviewOutput};

    let (mut app, first, second, dir) = file_preview_app(Some("cat"));
    let _ = app.ensure_file_browser_preview();
    assert_eq!(
        app.file_browser.preview_target.as_deref(),
        Some(first.as_str())
    );
    let _ = app.update(Message::FileBrowserPreviewDue(first.clone()));
    assert!(app.file_browser.preview_inflight);

    // Moving on while the first run is out: the second waits its debounce,
    // and a late debounce for an entry no longer selected is ignored.
    let _ = app.update(Message::MoveDown);
    assert_eq!(
        app.file_browser.preview_target.as_deref(),
        Some(second.as_str())
    );
    let _ = app.update(Message::FileBrowserPreviewDue(first.clone()));
    let _ = app.update(Message::FileBrowserPreviewDue(second.clone()));
    assert!(app.file_browser.preview_due && app.file_browser.preview_inflight);

    // The first result is cached and the queued run for the second starts.
    let _ = app.update(Message::FileBrowserPreview(
        FilePreviewKey::of(&first),
        Ok(Some(FilePreviewOutput::Text("\x1b[1mfirst\x1b[0m".into()))),
    ));
    assert!(app.file_browser.preview_inflight);
    let _ = app.update(Message::FileBrowserPreview(
        FilePreviewKey::of(&second),
        Ok(None),
    ));
    assert!(!app.file_browser.preview_inflight);

    // Ok(None) keeps the built-in text preview.
    let preview = app.resolve_selected_preview().unwrap();
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::Text { content, .. }] if content.contains("second.txt contents")
    ));

    // Going back to a cached entry swaps in the command output without a rerun.
    let _ = app.update(Message::MoveUp);
    assert_eq!(
        app.file_browser.preview_target.as_deref(),
        Some(first.as_str())
    );
    assert!(!app.file_browser.preview_due);
    let preview = app.resolve_selected_preview().unwrap();
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { heading: None, content }] if content.contains("first")
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn test_file_preview_command_rich_output_and_errors_overlay_builtin_preview() {
    use super::file_preview::{parse_output, FilePreviewKey};

    let (mut app, first, second, dir) = file_preview_app(Some("cat"));
    let rich = parse_output(
        true,
        br#"{"badge":"PDF","text":"page one","metadata":[["Pages","3"]]}"#,
    );
    let _ = app.update(Message::FileBrowserPreview(
        FilePreviewKey::of(&first),
        rich,
    ));

    let preview = app.resolve_selected_preview().unwrap();
    assert_eq!(preview.badge.as_deref(), Some("PDF"));
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::AnsiText { content, .. }] if content == "page one"
    ));
    assert!(preview
        .metadata
        .iter()
        .any(|(label, value)| label == "Pages" && value == "3"));
    assert!(preview.metadata.iter().any(|(label, _)| label == "Path"));

    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::FileBrowserPreview(
        FilePreviewKey::of(&second),
        Err("preview command timed out after 3s".into()),
    ));
    let preview = app.resolve_selected_preview().unwrap();
    assert_eq!(preview.badge.as_deref(), Some("File"));
    assert!(matches!(
        preview.bodies.as_slice(),
        [PreviewBody::Text { content, .. }] if content.contains("second.txt contents")
    ));
    assert!(preview
        .metadata
        .iter()
        .any(|(label, value)| label == "Preview" && value.contains("timed out")));
    std::fs::remove_dir_all(dir).unwrap();
}

fn pass_app() -> LauncherApp {
    use super::vault::{Outcome, Report};
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged("pass ".into()));
    assert!(task.units() > 0, "the entries load");
    let items = vec![
        super::pass::command_item(
            "pass:entry:web/github",
            "web/github",
            "web",
            "dialog-password",
        ),
        super::pass::command_item(super::pass::LOCK_IDENTITY, "Lock", "", "dialog-password"),
    ];
    let generation = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        generation,
        Report {
            outcome: Outcome::Loaded(items),
            notice: None,
        },
    ));
    assert_eq!(command_list_titles(&app), vec!["web/github", "Lock"]);
    app
}

fn vault_report(outcome: super::vault::Outcome) -> super::vault::Report {
    super::vault::Report {
        outcome,
        notice: None,
    }
}

fn ready_job() -> super::vault::Outcome {
    use super::vault::{EntryAction, Field, JobSettings, Outcome, SecretJob};
    Outcome::Ready(SecretJob::fetched(
        EntryAction::Copy(Field::Password),
        crate::secret::Secret::new("hunter2".into()),
        JobSettings {
            clipboard_timeout: 45,
            type_delay: std::time::Duration::ZERO,
        },
    ))
}

/// Select the pass entry and get asked for the passphrase.
fn pass_prompting() -> LauncherApp {
    use super::vault::{Outcome, Purpose};
    let mut app = pass_app();
    select_identity(&mut app, "pass:entry:web/github");
    assert!(
        app.update(Message::Submit).units() > 0,
        "the entry decrypts"
    );
    let generation = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        generation,
        vault_report(Outcome::Prompt(Purpose::GpgPassphrase, None)),
    ));
    assert!(app.vault.prompt.is_some());
    app
}

#[test]
fn test_vault_prompt_keeps_the_passphrase_out_of_the_query_and_logs() {
    let mut app = pass_prompting();
    assert_eq!(
        app.command_list.status(),
        Some("Enter the GPG passphrase and press Enter (Escape cancels)")
    );
    let typed = Message::PromptChanged(crate::secret::Secret::new("hunter2".into()));
    assert!(!format!("{typed:?}").contains("hunter2"));
    let _ = app.update(typed);
    assert_eq!(app.search_query, "pass ");
    assert!(!format!("{:?}", app.vault).contains("hunter2"));

    // The list and the query are frozen while the prompt is open.
    let selected = app.selected_index;
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::SearchChanged("pass x".into()));
    let _ = app.update(Message::HistoryPrevious);
    assert_eq!(app.selected_index, selected);
    assert_eq!(app.search_query, "pass ");

    // Enter sends the passphrase once and empties the field.
    assert!(app.update(Message::Submit).units() > 0);
    let prompt = app.vault.prompt.as_ref().unwrap();
    assert!(prompt.buffer.is_empty());
    assert_eq!(app.command_list.status(), Some("Decrypting..."));
    assert_eq!(app.update(Message::Submit).units(), 0, "ignored while busy");
}

#[test]
fn test_vault_wrong_passphrase_asks_again() {
    use super::secret_io::SecretError;
    use super::vault::{Outcome, Purpose};
    let mut app = pass_prompting();
    let _ = app.update(Message::PromptChanged(crate::secret::Secret::new(
        "wrong".into(),
    )));
    let _ = app.update(Message::Submit);
    let generation = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        generation,
        vault_report(Outcome::Prompt(
            Purpose::GpgPassphrase,
            Some(SecretError::BadPassphrase),
        )),
    ));
    let prompt = app.vault.prompt.as_ref().unwrap();
    assert!(prompt.buffer.is_empty());
    assert_eq!(
        app.command_list.status(),
        Some("Bad passphrase, try again (Escape cancels)")
    );

    // The right one resumes the copy, which runs once Raffi has closed.
    let _ = app.update(Message::PromptChanged(crate::secret::Secret::new(
        "right".into(),
    )));
    let _ = app.update(Message::Submit);
    let generation = app.vault.generation;
    let _ = app.update(Message::VaultDone(generation, vault_report(ready_job())));
    assert!(app.vault.prompt.is_none());
    assert!(matches!(
        *app.deferred_action.lock().unwrap(),
        Some(DeferredAction::Secret(_))
    ));
}

#[test]
fn test_vault_escape_closes_the_prompt_and_drops_the_late_result() {
    let mut app = pass_prompting();
    let _ = app.update(Message::PromptChanged(crate::secret::Secret::new(
        "secret".into(),
    )));
    let _ = app.update(Message::Submit);
    let generation = app.vault.generation;

    assert_eq!(app.update(Message::Cancel).units(), 0, "Raffi stays open");
    assert!(app.vault.prompt.is_none());
    assert_eq!(app.command_list.addon, Some(CommandListAddon::Pass));
    assert_eq!(command_list_titles(&app), vec!["web/github", "Lock"]);

    let _ = app.update(Message::VaultDone(generation, vault_report(ready_job())));
    assert!(app.deferred_action.lock().unwrap().is_none());
}

#[test]
fn test_vault_lock_and_leaving_the_list_drop_a_running_fetch() {
    let mut app = pass_app();
    select_identity(&mut app, "pass:entry:web/github");
    let _ = app.update(Message::Submit);
    let fetch = app.vault.generation;
    select_identity(&mut app, super::pass::LOCK_IDENTITY);
    assert!(
        app.update(Message::Submit).units() > 0,
        "Lock runs while busy"
    );
    let _ = app.update(Message::VaultDone(fetch, vault_report(ready_job())));
    assert!(app.deferred_action.lock().unwrap().is_none());

    let mut app = pass_app();
    select_identity(&mut app, "pass:entry:web/github");
    let _ = app.update(Message::Submit);
    let fetch = app.vault.generation;
    let _ = app.update(Message::SearchChanged("fire".into()));
    assert!(!app.vault.busy());
    let _ = app.update(Message::VaultDone(fetch, vault_report(ready_job())));
    assert!(app.deferred_action.lock().unwrap().is_none());
}

#[test]
fn test_vault_shortcuts_pick_the_action_and_tools_are_checked() {
    use super::vault::{EntryAction, Field, OpKind};
    let mut app = pass_app();
    select_identity(&mut app, "pass:entry:web/github");
    let _ = app.update(Message::FileBrowserOpenSelected);
    assert_eq!(
        app.vault.running.as_ref().map(|op| op.kind.clone()),
        Some(OpKind::Entry(
            EntryAction::Copy(Field::Otp),
            "web/github".into()
        ))
    );

    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |program| program != "rbw";
    assert!(
        app.update(Message::SearchChanged("bw ".into())).units() > 0,
        "the list scrolls to the status row"
    );
    assert_eq!(app.command_list.status(), Some("rbw not found"));
}

#[test]
fn test_vault_failed_action_keeps_the_selected_entry() {
    use super::secret_io::SecretError;
    use super::vault::{EntryAction, Field, OpKind, Outcome};
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |_| true;
    let _ = app.update(Message::SearchChanged("bw ".into()));
    let items = ["github", "bank"]
        .map(|name| {
            super::pass::command_item(&format!("bw:item:{name}"), name, "", "dialog-password")
        })
        .to_vec();
    let load = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        load,
        vault_report(Outcome::Loaded(items)),
    ));
    select_identity(&mut app, "bw:item:bank");

    let _ = app.update(Message::FileBrowserOpenSelected);
    let fetch = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        fetch,
        vault_report(Outcome::Failed(SecretError::NoOtp)),
    ));
    assert_eq!(command_list_titles(&app)[0], SecretError::NoOtp.message());

    // The error row pushed the rows down; Enter still copies from bank.
    let _ = app.update(Message::Submit);
    assert_eq!(
        app.vault.running.as_ref().map(|op| op.kind.clone()),
        Some(OpKind::Entry(
            EntryAction::Copy(Field::Password),
            "bank".into()
        ))
    );
}

#[test]
fn test_vault_locked_bitwarden_unlocks_with_rbw_then_escape_cancels() {
    use super::vault::{OpKind, Outcome};
    let mut app = test_app(AddonsConfig::default());
    app.program_exists = |_| true;
    assert!(app.update(Message::SearchChanged("bw ".into())).units() > 0);
    let load = app.vault.generation;

    // A locked vault restarts the load behind rbw's pinentry.
    assert!(
        app.update(Message::VaultDone(load, vault_report(Outcome::NeedsUnlock)))
            .units()
            > 0
    );
    assert!(app.vault.unlocking);
    assert_eq!(
        app.vault.running.as_ref().map(|op| op.kind.clone()),
        Some(OpKind::Load)
    );
    assert_eq!(
        app.command_list.status(),
        Some("Unlocking with rbw: answer the pinentry window (Escape cancels)")
    );

    // Escape stops the unlock and leaves the mode instead of closing Raffi;
    // a late result is dropped.
    let unlock = app.vault.generation;
    let _ = app.update(Message::Cancel);
    assert!(!app.vault.busy());
    assert!(!app.vault.unlocking);
    assert_eq!(app.command_list.addon, None);
    let _ = app.update(Message::VaultDone(
        unlock,
        vault_report(Outcome::Loaded(Vec::new())),
    ));
    assert!(command_list_titles(&app).is_empty());
}

/// The Bitwarden list with these logins, as `rbw list --raw` gives them.
fn bw_app(list: &str) -> LauncherApp {
    use super::vault::Outcome;
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    app.program_exists = |_| true;
    let _ = app.update(Message::SearchChanged("bw ".into()));
    let items = super::bitwarden::parse_items(list.as_bytes(), &Default::default()).unwrap();
    let load = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        load,
        vault_report(Outcome::Loaded(items)),
    ));
    app
}

const BW_LIST: &str = r#"[
    {"id":"bank","name":"bank","user":"me","folder":null,"uris":["https://mybank.example"],"type":"Login"},
    {"id":"gh","name":"code host","user":"me","folder":null,
     "uris":["androidapp://com.github.android","https://accounts.github.com/login"],"type":"Login"},
    {"id":"lab","name":"gitlab","user":"me","folder":null,"uris":["https://gitlab.com"],"type":"Login"}
]"#;

#[test]
fn test_vault_pasted_url_lists_the_site_first() {
    let mut app = bw_app(BW_LIST);
    let _ = app.update(Message::SearchChanged(
        "bw https://login.github.com/session?return=1".into(),
    ));
    assert_eq!(command_list_titles(&app), vec!["code host"]);

    // A bare domain works too, and fuzzy matches follow the site's rows.
    let _ = app.update(Message::SearchChanged("bw gitlab.com".into()));
    assert_eq!(command_list_titles(&app)[0], "gitlab");
}

#[test]
fn test_vault_query_change_snaps_to_first_result() {
    let mut app = bw_app(BW_LIST);
    let task = app.update(Message::SearchChanged("bw ba".into()));

    assert!(task.units() > 0, "the list scrolls to its first result");
    assert_eq!(command_list_titles(&app)[0], "bank");
    assert_eq!(app.selected_index, 0);

    app.command_list.items.truncate(1);
    let task = app.update(Message::SearchChanged("bw an".into()));
    assert!(task.units() > 0, "a single row still resets scrolling");
    app.command_list.items.clear();
    let task = app.update(Message::SearchChanged("bw zz".into()));
    assert!(task.units() > 0, "an empty list still resets scrolling");
}

#[test]
fn test_vault_notes_search_adds_rows_below_the_matches() {
    use std::collections::HashSet;
    let mut app = bw_app(BW_LIST);
    let _ = app.update(Message::SearchChanged("bw ba".into()));
    assert_eq!(app.vault.search_query, None, "too short to search");

    let _ = app.update(Message::SearchChanged("bw ban".into()));
    assert_eq!(app.vault.search_query.as_deref(), Some("ban"));
    let first = app.vault.search_generation;
    let _ = app.update(Message::SearchChanged("bw bank".into()));
    let second = app.vault.search_generation;
    assert_ne!(first, second);

    // A result for an older filter is dropped.
    let found = HashSet::from(["bw:item:lab".to_string(), "bw:item:bank".to_string()]);
    let _ = app.update(Message::VaultSearchDone(first, found.clone()));
    assert_eq!(command_list_titles(&app), vec!["bank"]);

    let selected = app.selected_index;
    let _ = app.update(Message::VaultSearchDone(second, found));
    assert_eq!(command_list_titles(&app), vec!["bank", "gitlab"]);
    assert_eq!(app.selected_index, selected);
    assert!(!app.command_list.is_extra_row(0), "bank matched by name");
    assert!(app.command_list.is_extra_row(1));

    // Another filter drops the rows it found.
    let _ = app.update(Message::SearchChanged("bw bankq".into()));
    assert!(command_list_titles(&app).is_empty());
}

#[test]
fn test_vault_notes_search_is_dropped_with_the_list() {
    use super::vault::Outcome;
    let mut app = bw_app(BW_LIST);
    let _ = app.update(Message::SearchChanged("bw recovery".into()));
    let stale = app.vault.search_generation;
    assert!(app.vault.search_query.is_some());

    let _ = app.update(Message::SearchChanged("fire".into()));
    assert_eq!(app.vault.search_query, None);

    // Back in the list, the search restarts once the rows load and the
    // late result of the first one is dropped.
    let _ = app.update(Message::SearchChanged("bw recovery".into()));
    assert_eq!(app.vault.search_query, None, "not while loading");
    let items = super::bitwarden::parse_items(BW_LIST.as_bytes(), &Default::default()).unwrap();
    let load = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        load,
        vault_report(Outcome::Loaded(items)),
    ));
    assert!(app.vault.search_query.is_some());
    assert_ne!(app.vault.search_generation, stale);
    let _ = app.update(Message::VaultSearchDone(
        stale,
        std::collections::HashSet::from(["bw:item:bank".to_string()]),
    ));
    assert!(command_list_titles(&app).is_empty());
}

#[test]
fn test_vault_recently_used_entries_come_first() {
    use super::vault::Outcome;
    let mut app = bw_app(BW_LIST);
    app.sort_mode = SortMode::Recency;
    assert_eq!(
        command_list_titles(&app),
        vec!["bank", "code host", "gitlab"]
    );
    select_identity(&mut app, "bw:item:lab");
    let _ = app.update(Message::Submit);
    let fetch = app.vault.generation;
    let _ = app.update(Message::VaultDone(fetch, vault_report(ready_job())));
    assert!(app.vault.usage.contains_key("bw:item:lab"));

    // Opened again, the copied item leads; Lock and Sync stay last.
    let _ = app.update(Message::SearchChanged("fire".into()));
    let _ = app.update(Message::SearchChanged("bw ".into()));
    let mut items = super::bitwarden::parse_items(BW_LIST.as_bytes(), &Default::default()).unwrap();
    items.push(super::pass::command_item(
        super::bitwarden::LOCK_IDENTITY,
        "Lock vault",
        "",
        "dialog-password",
    ));
    let load = app.vault.generation;
    let _ = app.update(Message::VaultDone(
        load,
        vault_report(Outcome::Loaded(items)),
    ));
    assert_eq!(
        command_list_titles(&app),
        vec!["gitlab", "bank", "code host", "Lock vault"]
    );
}

fn browser_tabs_app(query: &str) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the tabs load");
    assert_eq!(app.command_list.status(), Some("Loading browser tabs..."));
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(
        generation,
        Ok(super::browser_tabs::fixture_items()),
    ));
    app
}

#[test]
fn test_route_query_browser_tabs_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("tabs github", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::BrowserTabs,
            query: "github".into()
        }
    );
    assert_eq!(loggable_query("tabs github", &addons), "<redacted>");
    assert_eq!(
        keyword_suggestions("tab", &addons)[0].subtitle,
        "Switch to or close a browser tab"
    );

    addons.browser_tabs.trigger = Some("t".into());
    assert!(matches!(
        route_query("t x", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::BrowserTabs,
            ..
        }
    ));
    addons.browser_tabs.enabled = false;
    assert_eq!(route_query("t x", &addons), QueryMode::Standard);
}

#[test]
fn test_browser_tabs_show_the_page_preview_of_the_selected_tab() {
    let mut app = browser_tabs_app("tabs ");
    assert!(app.view.show_preview);
    let raffi = "https://github.com/chmouel/raffi".to_string();
    assert_eq!(app.command_list.preview_inflight, Some(raffi.clone()));
    let body = super::preview::PreviewBody::Text {
        heading: None,
        content: "Fast launcher".into(),
        truncated: false,
    };

    let _ = app.update(Message::MoveDown);
    let session = app.command_list.session;
    let _ = app.update(Message::TabPreviewLoaded(
        session.wrapping_add(1),
        raffi.clone(),
        Some(vec![body.clone()]),
    ));
    assert!(app.command_list.previews.is_empty(), "stale session");

    let _ = app.update(Message::TabPreviewLoaded(
        session,
        raffi,
        Some(vec![body.clone()]),
    ));
    assert_eq!(
        app.command_list.preview_inflight.as_deref(),
        Some("https://mail.example.com/inbox"),
        "the row selected meanwhile is looked up next"
    );
    assert!(app.resolve_selected_preview().unwrap().bodies.is_empty());
    let _ = app.update(Message::MoveUp);
    assert_eq!(app.resolve_selected_preview().unwrap().bodies, vec![body]);

    let wanted = app.command_list.preview_wanted.clone();
    assert!(wanted.is("https://github.com/chmouel/raffi"));
    let _ = app.update(Message::TogglePreview);
    assert!(
        !wanted.is("https://github.com/chmouel/raffi"),
        "a hidden preview stops pending lookups"
    );
}

#[test]
fn test_browser_tabs_enter_switches_after_close() {
    let mut app = browser_tabs_app("tabs ");
    assert_eq!(command_list_titles(&app), vec!["Raffi", "Mail"]);
    let _ = app.update(Message::SearchChanged("tabs mail.example".into()));
    assert_eq!(command_list_titles(&app), vec!["Mail"]);

    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::BrowserTab(super::browser_tabs::TabAction {
            tab_id: 2,
            operation: super::browser_tabs::TabOperation::Focus,
            browser_pid: Some(9),
            ..
        }))
    ));
}

#[test]
fn test_browser_tabs_ctrl_enter_closes_and_refreshes() {
    let mut app = browser_tabs_app("tabs ");
    let identity = "10:1".to_string();
    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "the tab closes in the background");
    assert!(deferred_action(&app).is_none(), "Raffi stays open");
    assert!(app.in_flight.is_pending("browser_tabs", &identity));

    let task = app.update(Message::CommandListActionDone(
        CommandListAddon::BrowserTabs,
        app.command_list.session,
        identity.clone(),
        Err("the browser is no longer connected".into()),
    ));
    assert!(task.units() > 0, "the list refreshes");
    assert_eq!(
        command_list_titles(&app)[0],
        "Raffi failed: the browser is no longer connected"
    );
    assert!(!app.in_flight.is_pending("browser_tabs", &identity));
}

#[test]
fn test_browser_tabs_close_selects_the_next_tab() {
    let mut app = browser_tabs_app("tabs ");
    let mut items = super::browser_tabs::fixture_items();
    let mut docs = items[1].clone();
    docs.identity = "10:3".into();
    docs.title = "Docs".into();
    items.push(docs);
    let without = |items: &[super::command_list::CommandItem], identity: &str| {
        items
            .iter()
            .filter(|item| item.identity != identity)
            .cloned()
            .collect::<Vec<_>>()
    };
    let close_selected = |app: &mut LauncherApp, rows: Vec<super::command_list::CommandItem>| {
        let identity = app
            .command_list
            .item(app.selected_index)
            .unwrap()
            .identity
            .clone();
        let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
        let _ = app.update(Message::Submit);
        let _ = app.update(Message::CommandListActionDone(
            CommandListAddon::BrowserTabs,
            app.command_list.session,
            identity,
            Ok(()),
        ));
        assert!(app.command_list.refreshing);
        let generation = app.command_list.generation;
        let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));
    };

    let generation = app.command_list.begin_refresh();
    let _ = app.update(Message::CommandListLoaded(generation, Ok(items.clone())));
    assert_eq!(command_list_titles(&app), vec!["Raffi", "Mail", "Docs"]);
    let _ = app.update(Message::MoveDown);
    assert_eq!(app.selected_index, 1);

    let rows = without(&items, "10:2");
    close_selected(&mut app, rows.clone());
    assert_eq!(command_list_titles(&app), vec!["Raffi", "Docs"]);
    assert_eq!(app.selected_index, 1, "the next tab is selected");

    close_selected(&mut app, without(&rows, "10:3"));
    assert_eq!(
        app.selected_index, 0,
        "closing the last tab selects the new last"
    );
}

#[test]
fn test_browser_tabs_ctrl_tab_switches_to_recent() {
    let mut app = browser_tabs_app("tabs ");
    assert_eq!(app.cycle_view_label().as_deref(), Some("By browser"));
    let _ = app.update(Message::MoveDown);
    assert_eq!(command_list_titles(&app), vec!["Raffi", "Mail"]);

    let task = app.update(Message::CycleView(false));
    assert!(task.units() > 0, "the tabs reload in the new order");
    assert_eq!(app.cycle_view_label().as_deref(), Some("Recent"));
    assert_eq!(
        app.command_list.status(),
        None,
        "the rows stay usable while the new order loads"
    );
    let mut rows = super::browser_tabs::fixture_items();
    rows.reverse();
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));
    assert_eq!(command_list_titles(&app), vec!["Mail", "Raffi"]);
    assert_eq!(app.selected_index, 0, "the same tab stays selected");

    let _ = app.update(Message::CycleView(true));
    assert_eq!(app.cycle_view_label().as_deref(), Some("By browser"));
    let _ = app.update(Message::CycleView(false));
    let _ = app.update(Message::SearchChanged(String::new()));
    let app = browser_tabs_app("tabs ");
    assert_eq!(
        app.cycle_view_label().as_deref(),
        Some("By browser"),
        "entering the list again starts grouped by browser"
    );
}

fn main_view_app(entries: &[&str], configs: Vec<RaffiConfig>, addons: AddonsConfig) -> LauncherApp {
    let (mut app, _) = LauncherApp::new(
        configs,
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
    app.text_snippets.mru_map.clear();
    app.text_snippets.mru_path = None;
    app.history.max_items = 0;
    app.program_exists = |_| true;
    let entries: Vec<String> = entries.iter().map(|entry| entry.to_string()).collect();
    let _ = app.configure_main_view(Some(&entries));
    app
}

#[test]
fn test_main_view_resolves_kill_and_enabled_slack_in_order() {
    use super::main_view::{Entry, SlackSource};

    let mut addons = AddonsConfig::default();
    addons.kill.enabled = true;
    addons.slack.enabled = true;
    let entries = [
        "kill",
        "slack",
        "slack.unread",
        "slack.threads",
        "slack.unread_threads",
        "slack.activity",
        "slack.actions",
        "slack.directs",
        "slack.dms",
        "slack.channels",
    ]
    .map(String::from);
    let resolved = super::main_view::resolve(&entries, &addons);
    assert_eq!(
        resolved,
        vec![
            Entry::List(CommandListAddon::Kill),
            Entry::Slack(SlackSource::Default),
            Entry::Slack(SlackSource::Unread),
            Entry::Slack(SlackSource::Threads),
            Entry::Slack(SlackSource::Activity),
            Entry::Slack(SlackSource::Actions),
            Entry::Slack(SlackSource::Directs),
            Entry::Slack(SlackSource::Channels),
        ]
    );

    addons.slack.enabled = false;
    let resolved = super::main_view::resolve(&entries, &addons);
    assert_eq!(resolved, vec![Entry::List(CommandListAddon::Kill)]);
}

#[test]
fn test_main_view_lists_kill_processes_for_a_query() {
    let mut addons = AddonsConfig::default();
    addons.kill.enabled = true;
    let mut app = main_view_app(&["kill"], Vec::new(), addons);
    let generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => {
            assert!(list.loading, "Kill starts its main-view loader");
            list.generation
        }
        _ => panic!("Kill command list source"),
    };
    let items = super::kill::fixture_items();
    let query = items[0]
        .title
        .split_whitespace()
        .next()
        .expect("a process title")
        .to_string();
    let _ = app.update(Message::MainViewLoaded(0, generation, Ok(items)));
    let _ = app.update(Message::SearchChanged(query));

    assert!(app.main_view.source_rows(0) > 0);
}

#[test]
fn test_main_view_shows_slack_home_rows_without_a_query() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = main_view_app(&["slack"], Vec::new(), addons);
    let generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack command list source"),
    };

    let _ = app.update(Message::MainViewLoaded(
        0,
        generation,
        Ok(super::browser_tabs::fixture_items()),
    ));

    assert!(app.main_view.shows(0));
    assert_eq!(app.main_view.source_rows(0), 2);
}

#[test]
fn test_main_view_slack_search_is_debounced_and_drops_stale_searches() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = main_view_app(&["slack"], Vec::new(), addons);
    let home_generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack command list source"),
    };

    let _ = app.update(Message::SearchChanged("d".into()));
    let short_query_generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack command list source"),
    };
    assert_eq!(short_query_generation, home_generation);

    let _ = app.update(Message::SearchChanged("deploy".into()));
    let search_generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack command list source"),
    };
    assert_ne!(search_generation, home_generation);

    assert_eq!(
        app.update(Message::MainViewSlackSearchDue(
            0,
            home_generation,
            "deploy".into(),
        ))
        .units(),
        0,
        "a stale source generation is ignored"
    );
    assert!(
        app.update(Message::MainViewSlackSearchDue(
            0,
            search_generation,
            "deploy".into(),
        ))
        .units()
            > 0,
        "the current query starts the Slack search"
    );
}

#[test]
fn test_main_view_slack_search_hands_off_in_search_view() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = main_view_app(&["slack"], Vec::new(), addons);
    let _ = app.update(Message::SearchChanged("deploy".into()));
    let generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack command list source"),
    };
    let mut item = super::command_list::CommandItem::info(
        "slack:message:fixture",
        "Deployment update".into(),
        "mail-message-new",
    );
    item.match_text = "deployment deploy".into();
    item.primary = Some(super::command_list::CommandAction {
        action: super::actions::DeferredAction::Slack(super::slack::SlackAction::Open(
            "slack://channel?team=T1&id=C1".into(),
        )),
        confirm: None,
        keep_open: false,
        progress: None,
    });
    let _ = app.update(Message::MainViewLoaded(0, generation, Ok(vec![item])));

    let _ = app.activate_main_view(super::main_view::Block::BeforeLaunchers, 0);

    assert_eq!(app.search_query, "slack search deploy");
    assert_eq!(app.command_list.addon, Some(CommandListAddon::Slack));
}

fn slack_fixture_item(
    identity: &str,
    title: &str,
    sort_priority: u8,
    dimmed: bool,
) -> super::command_list::CommandItem {
    let mut item =
        super::command_list::CommandItem::info(identity, title.into(), "mail-message-new");
    item.match_text = title.to_lowercase();
    item.sort_priority = sort_priority;
    item.dimmed = dimmed;
    item.primary = Some(super::command_list::CommandAction {
        action: super::actions::DeferredAction::Slack(super::slack::SlackAction::Open(
            "slack://channel?team=T1&id=C1".into(),
        )),
        confirm: None,
        keep_open: false,
        progress: None,
    });
    item
}

#[test]
fn test_slack_open_closes_by_default_but_in_place_actions_stay_open() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    let open = slack_fixture_item("slack:conv:C1", "general", 0, false);
    let mut snooze = super::command_list::CommandItem::info(
        "slack:action:end-snooze",
        "Resume notifications".into(),
        "slack",
    );
    snooze.primary = Some(super::command_list::CommandAction {
        action: super::actions::DeferredAction::Slack(super::slack::SlackAction::EndSnooze),
        confirm: None,
        keep_open: false,
        progress: None,
    });
    let keep_open = |app: &LauncherApp| {
        let mut rows = vec![open.clone(), snooze.clone()];
        app.apply_slack_keep_open(&mut rows);
        rows.iter()
            .map(|row| row.primary.as_ref().unwrap().keep_open)
            .collect::<Vec<_>>()
    };

    assert_eq!(keep_open(&app), [false, true]);

    app.close_on_action = Some(false);
    assert_eq!(keep_open(&app), [true, true]);
}

#[test]
fn test_main_view_slack_hides_muted_and_cold_channels_until_queried_or_focused() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = main_view_app(&["slack"], Vec::new(), addons);
    app.slack.usage_loaded = true;
    app.slack.usage.insert(
        "slack:conv:C_RECENT".into(),
        super::support::MruEntry {
            count: 3,
            last_used: 100,
        },
    );
    app.slack.usage.insert(
        "slack:conv:C_MUTED_RECENT".into(),
        super::support::MruEntry {
            count: 10,
            last_used: 200,
        },
    );

    let generation = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack command list source"),
    };
    let items = vec![
        slack_fixture_item("slack:conv:C_COLD", "#cold-channel", 0, false),
        slack_fixture_item("slack:conv:C_MUTED_RECENT", "#muted-recent", 0, true),
        slack_fixture_item("slack:conv:C_RECENT", "#recent-channel", 0, false),
        slack_fixture_item("slack:conv:C_UNREAD", "#unread-channel", 1, false),
        slack_fixture_item("slack:user:U_ALICE", "Alice", 0, false),
    ];
    let _ = app.update(Message::MainViewLoaded(0, generation, Ok(items)));

    let visible_titles = |app: &LauncherApp| -> Vec<String> {
        let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
            panic!("Slack list source");
        };
        (0..app.main_view.source_rows(0))
            .filter_map(|row| list.item(row).map(|item| item.title.clone()))
            .collect()
    };

    assert_eq!(
        visible_titles(&app),
        vec!["#unread-channel", "#recent-channel", "Alice"],
        "cold and muted channels are hidden on an empty query in All"
    );

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view_focus_label(), "Slack");
    assert_eq!(
        visible_titles(&app),
        vec![
            "#unread-channel",
            "#recent-channel",
            "#cold-channel",
            "Alice",
            "#muted-recent",
        ],
        "focusing Slack via Ctrl+Tab reveals cold and muted channels, with muted demoted"
    );
}

#[test]
fn test_main_view_slack_subviews_filter_locally_and_hand_off_to_matching_subview() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = main_view_app(
        &["slack.threads", "slack.directs", "slack.channels"],
        Vec::new(),
        addons,
    );
    app.slack.usage_loaded = true;
    app.slack.usage.insert(
        "slack:conv:C_GENERAL".into(),
        super::support::MruEntry {
            count: 2,
            last_used: 100,
        },
    );

    for (source, rows) in [
        (
            0,
            vec![slack_fixture_item(
                "slack:thread:C1:1",
                "Release thread",
                0,
                false,
            )],
        ),
        (
            1,
            vec![
                slack_fixture_item("slack:user:U1", "Alice Direct", 0, false),
                slack_fixture_item("slack:conv:C_GENERAL", "#general", 0, false),
            ],
        ),
        (
            2,
            vec![
                slack_fixture_item("slack:user:U1", "Alice Direct", 0, false),
                slack_fixture_item("slack:conv:C_GENERAL", "#general", 0, false),
                slack_fixture_item("slack:conv:C_COLD", "#cold", 0, false),
            ],
        ),
    ] {
        let generation = match &app.main_view.sources[source] {
            super::main_view::Source::List(list) => list.generation,
            _ => panic!("Slack command list source"),
        };
        let _ = app.update(Message::MainViewLoaded(source, generation, Ok(rows)));
    }

    assert_eq!(app.main_view.source_rows(0), 1);
    assert_eq!(
        app.main_view.source_rows(1),
        1,
        "slack.directs keeps only users"
    );
    assert_eq!(
        app.main_view.source_rows(2),
        1,
        "slack.channels keeps only non-cold channels on empty query"
    );

    let threads_gen = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack threads source"),
    };
    let _ = app.update(Message::SearchChanged("release".into()));
    let after_gen = match &app.main_view.sources[0] {
        super::main_view::Source::List(list) => list.generation,
        _ => panic!("Slack threads source"),
    };
    assert_eq!(
        after_gen, threads_gen,
        "sub-views filter locally instead of scheduling message search"
    );
    assert_eq!(app.main_view.source_rows(0), 1);
    assert_eq!(app.main_view.source_rows(1), 0);

    let _ = app.activate_main_view(super::main_view::Block::BeforeLaunchers, 0);
    assert_eq!(app.search_query, "slack threads release");
    assert_eq!(app.command_list.addon, Some(CommandListAddon::Slack));
}

/// Deliver the browser tabs fixture to main view source 0.
fn load_main_view_tabs(app: &mut LauncherApp) {
    let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
        panic!("command list source");
    };
    let generation = list.generation;
    let _ = app.update(Message::MainViewLoaded(
        0,
        generation,
        Ok(super::browser_tabs::fixture_items()),
    ));
}

fn firefox_launcher() -> Vec<RaffiConfig> {
    vec![RaffiConfig {
        binary: Some("firefox".into()),
        description: Some("Firefox mail".into()),
        ..Default::default()
    }]
}

#[test]
fn test_main_view_without_launchers_lists_only_addon_rows() {
    let mut app = main_view_app(
        &["browser_tabs"],
        firefox_launcher(),
        AddonsConfig::default(),
    );
    load_main_view_tabs(&mut app);
    assert!(app.filtered_configs.is_empty(), "launchers are not listed");
    assert_eq!(keyword_names(&app), ["tabs"], "only the plugin row");
    assert_eq!(app.total_items(), 1, "addon rows wait for a query");

    let _ = app.update(Message::SearchChanged("mail".into()));
    assert!(app.filtered_configs.is_empty());
    assert!(app.keyword_suggestions.is_empty());
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.resolve_selected_preview().unwrap().title, "Mail");

    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "tabs mail", "handed to the tabs mode");
    assert!(
        app.main_view.stale.contains(&0),
        "reloaded when shown again"
    );
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::BrowserTab(super::browser_tabs::TabAction {
            tab_id: 2,
            ..
        }))
    ));
}

#[test]
fn test_main_view_groups_sources_in_config_order_and_caps_them() {
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Notes".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(Vec::new()),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: false,
        restore_clipboard: false,
        ..Default::default()
    });
    let mut app = main_view_app(
        &[
            "browser_tabs",
            "launchers",
            "text_snippets.Notes",
            "kill",
            "nope",
        ],
        firefox_launcher(),
        addons,
    );
    assert_eq!(
        app.main_view.sources.len(),
        3,
        "Kill is supported and unsupported entries are skipped"
    );
    assert_eq!(app.filtered_configs, vec![0], "launchers are listed");
    load_main_view_tabs(&mut app);
    let snippets = (0..8)
        .map(|index| TextSnippet {
            name: format!("mail {index}"),
            value: format!("value {index}"),
            keyword: None,
            icon: None,
        })
        .collect();
    let _ = app.update(Message::MainViewSnippetsLoaded(1, Ok(snippets)));

    let _ = app.update(Message::SearchChanged("mail".into()));
    use super::main_view::Block;
    assert_eq!(
        app.main_view.row_count(Block::BeforeLaunchers),
        1,
        "the tab"
    );
    assert_eq!(app.filtered_configs, vec![0]);
    assert_eq!(
        app.main_view.row_count(Block::AfterLaunchers),
        super::main_view::MAX_ROWS
    );
    assert_eq!(app.total_items(), 1 + 1 + super::main_view::MAX_ROWS);

    let _ = app.update(Message::MoveDown);
    assert!(app.resolve_selected_preview().is_none(), "the launcher row");
    let _ = app.update(Message::MoveDown);
    assert_eq!(app.resolve_selected_preview().unwrap().title, "mail 0");

    let _ = app.update(Message::SearchChanged("tabs mail".into()));
    assert_eq!(app.main_view.row_count(Block::BeforeLaunchers), 0);
    assert_eq!(app.main_view.row_count(Block::AfterLaunchers), 0);
}

#[test]
fn test_main_view_system_row_still_needs_a_second_enter() {
    let mut app = main_view_app(&["system"], Vec::new(), AddonsConfig::default());
    let _ = app.update(Message::SearchChanged("reboot".into()));
    assert_eq!(app.total_items(), 1);

    let task = app.update(Message::Submit);
    assert!(task.units() > 0);
    assert_eq!(app.command_list.addon, Some(CommandListAddon::System));
    assert!(app.command_list.armed.is_some());
    assert!(app.deferred_action.lock().unwrap().is_none());

    let _ = app.update(Message::Submit);
    assert_eq!(
        deferred_argv(&app),
        Some(vec!["systemctl".into(), "reboot".into()])
    );
}

#[test]
fn test_main_view_script_filter_waits_for_min_length_and_drops_stale_runs() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "Issues".into(),
        command: "issues".into(),
        keyword: "is".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: Some(3),
        dynamic_preview: None,
        ..Default::default()
    });
    let mut app = main_view_app(&["script_filters.Issues"], Vec::new(), addons);
    let result = |title: &str| ScriptFilterResult {
        default_icon: None,
        items: vec![ScriptFilterItem {
            title: title.into(),
            valid: true,
            subtitle: None,
            arg: None,
            indent: 0,
            icon: None,
            preview: None,
            quicklookurl: None,
            variables: Default::default(),
            text: None,
        }],
    };

    let task = app.update(Message::SearchChanged("ab".into()));
    assert_eq!(task.units(), 0, "too short to run");
    let task = app.update(Message::SearchChanged("abc".into()));
    assert!(task.units() > 0, "long enough to run");
    let task = app.update(Message::SearchChanged("abcd".into()));
    assert_eq!(task.units(), 0, "waits for the run that is out");
    let super::main_view::Source::ScriptFilter { generation, .. } = &app.main_view.sources[0]
    else {
        panic!("script filter source");
    };
    let latest = *generation;

    let task = app.update(Message::MainViewScriptFilter(
        0,
        latest - 1,
        Some(Ok(result("stale"))),
    ));
    assert_eq!(app.total_items(), 0, "a superseded run is dropped");
    assert!(task.units() > 0, "the latest query runs next");
    let _ = app.update(Message::MainViewScriptFilter(
        0,
        latest,
        Some(Ok(result("fresh"))),
    ));
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.resolve_selected_preview().unwrap().title, "fresh");
}

fn issues_filter() -> ScriptFilterConfig {
    ScriptFilterConfig {
        name: "Issues".into(),
        command: "issues".into(),
        keyword: "is".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
        dynamic_preview: None,
        ..Default::default()
    }
}

#[test]
fn test_main_view_late_rows_keep_the_selection() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(issues_filter());
    let mut app = main_view_app(
        &["script_filters.Issues", "launchers"],
        firefox_launcher(),
        addons,
    );
    let _ = app.update(Message::SearchChanged("mail".into()));
    assert_eq!(app.selected_index, 0, "the launcher, alone so far");
    let super::main_view::Source::ScriptFilter { generation, .. } = &app.main_view.sources[0]
    else {
        panic!("script filter source");
    };
    let run = *generation;
    let items = (0..2)
        .map(|index| ScriptFilterItem {
            title: format!("mail issue {index}"),
            valid: true,
            subtitle: None,
            arg: None,
            indent: 0,
            icon: None,
            preview: None,
            quicklookurl: None,
            variables: Default::default(),
            text: None,
        })
        .collect();
    let _ = app.update(Message::MainViewScriptFilter(
        0,
        run,
        Some(Ok(ScriptFilterResult {
            default_icon: None,
            items,
        })),
    ));
    assert_eq!(app.total_items(), 3);
    assert_eq!(app.selected_index, 2, "still on the launcher");
}

#[test]
fn test_main_view_launcher_previews_skip_rows_above_them() {
    let mut launchers = firefox_launcher();
    launchers.push(RaffiConfig {
        binary: Some("thunderbird".into()),
        description: Some("Thunderbird mail".into()),
        ..Default::default()
    });
    for launcher in &mut launchers {
        launcher.show_preview = Some(true);
    }
    let mut app = main_view_app(
        &["browser_tabs", "launchers"],
        launchers,
        AddonsConfig::default(),
    );
    load_main_view_tabs(&mut app);
    let _ = app.update(Message::SearchChanged("mail".into()));
    assert_eq!(app.total_items(), 3);
    let titles: Vec<String> = (0..3)
        .map(|index| {
            app.selected_index = index;
            app.resolve_selected_preview().unwrap().title
        })
        .collect();
    assert_eq!(titles[0], "Mail");
    assert_ne!(titles[1], titles[2], "each launcher shows its own preview");
}

#[test]
fn test_main_view_snippets_fill_in_placeholders() {
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Notes".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(Vec::new()),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: true,
        restore_clipboard: false,
        ..Default::default()
    });
    let mut app = main_view_app(&["text_snippets.Notes"], Vec::new(), addons);
    let snippet = |name: &str, value: &str| TextSnippet {
        name: name.into(),
        value: value.into(),
        keyword: None,
        icon: None,
    };
    let _ = app.update(Message::MainViewSnippetsLoaded(
        0,
        Ok(vec![
            snippet("Name", "Ada"),
            snippet("Signed", "{snippet:Name}!"),
        ]),
    ));
    let _ = app.update(Message::SearchChanged("signed".into()));
    assert_eq!(app.total_items(), 1);
    let preview = app.resolve_selected_preview().unwrap();
    assert!(
        format!("{:?}", preview.bodies).contains("Ada!"),
        "{:?}",
        preview.bodies
    );
}

fn issue_items(count: usize) -> ScriptFilterResult {
    ScriptFilterResult {
        default_icon: None,
        items: (0..count)
            .map(|index| ScriptFilterItem {
                title: format!("issue {index}"),
                valid: true,
                subtitle: None,
                arg: None,
                indent: 0,
                icon: None,
                preview: None,
                quicklookurl: None,
                variables: Default::default(),
                text: None,
            })
            .collect(),
    }
}

fn script_filter_generation(app: &LauncherApp, source: usize) -> u64 {
    let super::main_view::Source::ScriptFilter { generation, .. } = &app.main_view.sources[source]
    else {
        panic!("script filter source");
    };
    *generation
}

#[test]
fn test_main_view_selection_follows_launchers_moved_down() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(issues_filter());
    let mut app = main_view_app(
        &["launchers", "script_filters.Issues"],
        firefox_launcher(),
        addons,
    );
    let _ = app.update(Message::SearchChanged("ail".into()));
    assert_eq!(app.selected_index, 0, "the launcher, alone so far");
    let run = script_filter_generation(&app, 0);
    let mut result = issue_items(2);
    for item in &mut result.items {
        item.title = format!("ail {}", item.title);
    }
    let _ = app.update(Message::MainViewScriptFilter(0, run, Some(Ok(result))));
    assert_eq!(app.total_items(), 3);
    assert_eq!(app.selected_index, 2, "still on the launcher");
}

#[test]
fn test_main_view_selection_stays_in_range_when_its_rows_go() {
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(issues_filter());
    let mut app = main_view_app(
        &["launchers", "script_filters.Issues"],
        firefox_launcher(),
        addons,
    );
    let _ = app.update(Message::SearchChanged("mail".into()));
    let run = script_filter_generation(&app, 0);
    let _ = app.update(Message::MainViewScriptFilter(
        0,
        run,
        Some(Ok(issue_items(1))),
    ));
    let _ = app.update(Message::SearchChanged("mai".into()));
    let _ = app.update(Message::MoveDown);
    assert_eq!(app.selected_index, 1, "the earlier issue, still listed");

    let run = script_filter_generation(&app, 0);
    let _ = app.update(Message::MainViewScriptFilter(
        0,
        run,
        Some(Ok(issue_items(0))),
    ));
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.selected_index, 0, "back on the launcher");
}

#[test]
fn test_main_view_reloads_after_any_action_of_its_addon() {
    let mut app = main_view_app(&["browser_tabs"], Vec::new(), AddonsConfig::default());
    load_main_view_tabs(&mut app);
    let action_done = |app: &mut LauncherApp| {
        app.update(Message::CommandListActionDone(
            CommandListAddon::BrowserTabs,
            0,
            "tab".into(),
            Ok(()),
        ))
    };

    // Closed from the tabs mode, which the main view does not show.
    let _ = app.update(Message::SearchChanged("tabs mail".into()));
    let _ = action_done(&mut app);
    assert!(app.main_view.stale.contains(&0));
    let task = app.update(Message::SearchChanged("mail".into()));
    assert!(app.main_view.stale.is_empty());
    assert!(task.units() > 0, "reloaded as it shows again");

    // Done after the user came back: reload at once, dropping older loads.
    let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
        panic!("command list source");
    };
    let earlier = list.generation;
    let task = action_done(&mut app);
    assert!(task.units() > 0);
    let _ = app.update(Message::MainViewLoaded(0, earlier, Ok(Vec::new())));
    assert_eq!(app.total_items(), 1, "the older load is dropped");
}

#[test]
fn test_main_view_audio_sets_volume_only_with_a_prefix() {
    let mut app = main_view_app(&["audio"], Vec::new(), AddonsConfig::default());
    let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
        panic!("command list source");
    };
    let generation = list.generation;
    let _ = app.update(Message::MainViewLoaded(
        0,
        generation,
        Ok(super::audio::fixture_items(true)),
    ));

    let _ = app.update(Message::SearchChanged("vol 40".into()));
    assert_eq!(
        app.resolve_selected_preview().unwrap().title,
        "Set output volume to 40%"
    );
    let _ = app.update(Message::SearchChanged("40".into()));
    assert!(app
        .resolve_selected_preview()
        .is_none_or(|row| !row.title.starts_with("Set output volume")));

    let _ = app.update(Message::SearchChanged("vol 40".into()));
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "audio vol 40", "handed to the audio mode");
    assert!(app.in_flight.is_pending("audio", super::audio::SET_VOLUME));

    // The volume row keeps following the query after the handoff, even
    // when the action failed and the list did not reload.
    let _ = app.update(Message::CommandListActionDone(
        CommandListAddon::Audio,
        app.command_list.session,
        super::audio::SET_VOLUME.into(),
        Err("pactl failed".into()),
    ));
    let volume_row = |app: &LauncherApp| {
        let position = app.command_list.position(super::audio::SET_VOLUME)?;
        let index = app.command_list.filtered[position];
        Some(app.command_list.items[index].title.clone())
    };
    let _ = app.update(Message::SearchChanged("audio 50".into()));
    assert_eq!(
        volume_row(&app).as_deref(),
        Some("Set output volume to 50%")
    );
    let _ = app.update(Message::SearchChanged("audio razer".into()));
    assert_eq!(volume_row(&app), None);
}

/// Titles of the main view rows in display order.
fn keyword_names(app: &LauncherApp) -> Vec<&str> {
    app.keyword_suggestions
        .iter()
        .map(|suggestion| suggestion.keyword.as_str())
        .collect()
}

#[test]
fn test_main_view_offers_detected_plugins_before_apps() {
    let mut addons = AddonsConfig::default();
    addons.media.enabled = true;
    let mut app = main_view_app(&[], firefox_launcher(), addons);
    app.program_exists = |program| program != "playerctl";
    let entries = ["media", "system", "launchers"].map(String::from);
    let _ = app.configure_main_view(Some(&entries));
    assert_eq!(keyword_names(&app), ["sys"], "no playerctl, no media row");
    assert_eq!(app.selected_index, 0, "the first plugin row");
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(keyword_names(&app), ["sys"]);
    assert_eq!(app.total_items(), 2, "the plugin row, then the launcher");

    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "sys ", "Enter arms the keyword");

    let _ = app.update(Message::SearchChanged("fire".into()));
    assert!(app.keyword_suggestions.is_empty(), "a query lists matches");
}

#[test]
fn test_short_label_fits_chip() {
    use super::main_view::short_label;
    assert_eq!(short_label("Snippets", 12), "Snippets");
    assert_eq!(short_label("Search the Web", 12), "Search Web");
    assert_eq!(short_label("PAC Pull Requests", 12), "PAC Pull…");
    assert_eq!(short_label("Kubernetesclusters", 12), "Kubernetesc…");
}

#[test]
fn test_chip_label_names_plugins() {
    use super::main_view::chip_label;
    let mut addons = AddonsConfig::default();
    addons.script_filters.push(ScriptFilterConfig {
        name: "PAC Pull Requests".into(),
        command: "pac".into(),
        keyword: "pr".into(),
        icon: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        env: std::collections::HashMap::new(),
        min_query_length: None,
        dynamic_preview: None,
        ..Default::default()
    });
    assert_eq!(chip_label(addons.browser_tabs.trigger(), &addons), "Tabs");
    assert_eq!(chip_label(addons.terminal.trigger(), &addons), "Terminal");
    assert_eq!(chip_label("pr", &addons), "PAC Pull…");
    assert_eq!(chip_label("zz", &addons), "zz", "unknown keeps the keyword");
}

#[test]
fn test_main_view_chips_setting() {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = Some(true);
    let entries = ["system", "clipboard", "launchers"].map(String::from);

    let mut app = main_view_app(&[], firefox_launcher(), addons.clone());
    app.main_view.chips = crate::MainViewChips::Keyword;
    let _ = app.configure_main_view(Some(&entries));
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.chip_labels(), ["sys", "clip"]);

    let mut app = main_view_app(&[], firefox_launcher(), addons.clone());
    let _ = app.configure_main_view(Some(&entries));
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.chip_labels(), ["System", "Clipboard"]);

    let mut app = main_view_app(&[], firefox_launcher(), addons);
    app.main_view.chips = crate::MainViewChips::Off;
    let _ = app.configure_main_view(Some(&entries));
    let _ = app.update(Message::SearchChanged(String::new()));
    assert!(!app.keyword_chips);
    assert_eq!(app.total_items(), 1, "off starts with the apps");
}

#[test]
fn test_chip_window_keeps_active_chip_on_one_line() {
    use super::view::chip_window;
    let widths = [50.0; 9];
    assert_eq!(chip_window(&widths, 1000.0, 4), 0..9, "everything fits");
    // 3 chips + spacing + the "›" marker fit in 200, a 4th would not.
    assert_eq!(chip_window(&widths, 200.0, 0), 0..3);
    assert_eq!(chip_window(&widths, 200.0, 8), 6..9);
    for active in 0..widths.len() {
        let window = chip_window(&widths, 260.0, active);
        assert!(window.contains(&active), "{active} hidden in {window:?}");
        assert!(window.len() >= 2, "{active}: a neighbour stays in view");
    }
    assert_eq!(
        chip_window(&[500.0, 50.0], 100.0, 0),
        0..1,
        "too wide still shows"
    );
    assert_eq!(chip_window(&[], 100.0, 0), 0..0);
}

#[test]
fn test_main_view_offers_plugins_as_one_row_of_chips() {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = Some(true);
    let mut app = main_view_app(&[], firefox_launcher(), addons);
    app.program_exists = |program| program != "rbw";
    let entries = [
        "system",
        "clipboard",
        "bitwarden",
        "emoji",
        "terminal",
        "launchers",
    ]
    .map(String::from);
    let _ = app.configure_main_view(Some(&entries));
    assert_eq!(
        keyword_names(&app),
        ["sys", "clip", "emoji", ">"],
        "no rbw, no bitwarden chip"
    );
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.total_items(), 2, "the chips row, then the launcher");
    assert_eq!(
        app.main_view.stops,
        [
            super::main_view::Focus::Source(0),
            super::main_view::Focus::Launchers
        ],
        "keyword-only entries are no Ctrl+Tab stop"
    );

    let _ = app.update(Message::ChipMove(false));
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "clip ", "Enter arms the active chip");

    let _ = app.update(Message::SearchChanged(String::new()));
    let _ = app.update(Message::ChipMove(true));
    assert_eq!(app.chip_index, 3, "Left wraps to the last chip");
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::ChipMove(false));
    assert_eq!(app.chip_index, 3, "arrows only move chips on their row");
    let _ = app.update(Message::ChipScroll(true));
    assert_eq!(
        (app.selected_index, app.chip_index),
        (0, 2),
        "a ‹ click selects the row and moves left"
    );

    let _ = app.update(Message::ChipClicked(2));
    assert_eq!(app.search_query, "emoji ");

    let _ = app.update(Message::SearchChanged("fire".into()));
    assert!(!app.keyword_chips);
    assert_eq!(app.total_items(), 1, "typed queries get no clipboard rows");
}

#[test]
fn test_keyword_icon_falls_back_to_emoji() {
    let mut app = main_view_app(&[], Vec::new(), AddonsConfig::default());
    app.no_icons = false;
    app.icon_map.clear();
    assert!(matches!(
        app.keyword_icon(super::app::SYSTEM_ICON),
        Some(super::snippets::SnippetIcon::Glyph(glyph)) if glyph == "🔌"
    ));
    assert!(app.keyword_icon("no-such-icon").is_none());
}

fn main_view_titles(app: &LauncherApp) -> Vec<String> {
    use super::main_view::{Block, Source};
    [Block::BeforeLaunchers, Block::AfterLaunchers]
        .into_iter()
        .flat_map(|block| app.main_view.rows(block))
        .map(|(source, row)| match &app.main_view.sources[source] {
            Source::List(list) => list.item(row).unwrap().title.clone(),
            Source::Snippets {
                items, filtered, ..
            } => items[filtered[row]].name.clone(),
            Source::ScriptFilter { results, .. } => {
                results.as_ref().unwrap().items[row].title.clone()
            }
        })
        .collect()
}

#[test]
fn test_main_view_lists_groups_starting_with_the_query_first() {
    use super::main_view::Block;
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Notes".into(),
        keyword: "sn".into(),
        icon: None,
        snippets: Some(Vec::new()),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: false,
        restore_clipboard: false,
        ..Default::default()
    });
    let launchers = vec![RaffiConfig {
        binary: Some("brave".into()),
        description: Some("Brave Browser".into()),
        ..Default::default()
    }];
    let mut app = main_view_app(
        &["browser_tabs", "text_snippets.Notes", "launchers"],
        launchers,
        addons,
    );
    load_main_view_tabs(&mut app);
    let snippet = |name: &str, value: &str| TextSnippet {
        name: name.into(),
        value: value.into(),
        keyword: None,
        icon: None,
    };
    let _ = app.update(Message::MainViewSnippetsLoaded(
        1,
        Ok(vec![
            snippet("Meeting notes", "met brave people"),
            snippet("Babysit PR", "make a branch, review everything"),
        ]),
    ));

    let _ = app.update(Message::SearchChanged("brave".into()));
    assert_eq!(app.main_view.row_count(Block::BeforeLaunchers), 0);
    assert_eq!(app.filtered_configs, vec![0]);
    assert_eq!(app.selected_index, 0, "the launcher comes first");
    assert_eq!(
        main_view_titles(&app),
        vec!["Meeting notes"],
        "a scattered fuzzy match is dropped"
    );

    let _ = app.update(Message::SearchChanged("people".into()));
    assert_eq!(
        app.main_view.row_count(Block::BeforeLaunchers),
        1,
        "config order when nothing starts with the query"
    );
}

#[test]
fn test_main_view_tabs_need_every_query_word() {
    let mut app = main_view_app(
        &["browser_tabs", "launchers"],
        Vec::new(),
        AddonsConfig::default(),
    );
    load_main_view_tabs(&mut app);
    let _ = app.update(Message::SearchChanged("rail".into()));
    assert!(
        main_view_titles(&app).is_empty(),
        "r…a…i…l over Raffi's URL"
    );
    let _ = app.update(Message::SearchChanged("raffi github".into()));
    assert_eq!(main_view_titles(&app), vec!["Raffi"]);
}

fn recent_pick(source: &str, key: &str) -> super::recent::RecentPick {
    super::recent::RecentPick::new(source.into(), key.into())
}

#[test]
fn test_main_view_empty_query_lists_recent_picks() {
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(snippet_source(
        "Notes",
        "sn",
        vec![
            snippet("alpha", "a"),
            snippet("beta", "b"),
            snippet("gamma", "c"),
        ],
    ));
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
        dynamic_preview: None,
        ..Default::default()
    });
    let mut app = main_view_app(
        &[
            "browser_tabs",
            "launchers",
            "text_snippets.Notes",
            "script_filters.Search",
        ],
        firefox_launcher(),
        addons,
    );
    let mut moved_tab = recent_pick("browser_tabs", "10:99");
    moved_tab.url = Some("https://github.com/chmouel/raffi".into());
    let mut repo = recent_pick("script_filters.Search", "raffi");
    repo.item = Some(super::recent::StoredItem {
        title: "chmouel/raffi".into(),
        subtitle: None,
        arg: Some("raffi".into()),
        icon: None,
        variables: std::collections::HashMap::new(),
    });
    app.main_view.recent = vec![
        recent_pick("browser_tabs", "10:2"),
        recent_pick("text_snippets.Notes", "beta"),
        moved_tab,
        repo,
        recent_pick("text_snippets.Notes", "deleted"),
        recent_pick("text_snippets.Notes", "alpha"),
        recent_pick("text_snippets.Notes", "gamma"),
    ];
    let _ = app.update(Message::MainViewSnippetsLoaded(
        1,
        Ok(vec![
            snippet("alpha", "a"),
            snippet("beta", "b"),
            snippet("gamma", "c"),
        ]),
    ));
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(
        main_view_titles(&app),
        ["beta", "alpha", "chmouel/raffi"],
        "loading tabs keep their places"
    );

    load_main_view_tabs(&mut app);
    assert_eq!(
        main_view_titles(&app),
        ["Mail", "Raffi", "beta", "alpha", "chmouel/raffi"],
        "newest first per source, a gone snippet skipped, a tab found by its page"
    );
    assert_eq!(app.filtered_configs, vec![0], "launchers stay");
    let super::main_view::Source::ScriptFilter { searched, .. } = &app.main_view.sources[2] else {
        panic!("script filter source");
    };
    assert!(searched.is_empty(), "the script filter did not run");

    let _ = app.update(Message::SearchChanged("mail".into()));
    assert_eq!(main_view_titles(&app), ["Mail"], "typing lists matches");

    let _ = app.update(Message::SearchChanged(String::new()));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(
        main_view_titles(&app),
        ["Raffi", "Mail"],
        "a focused source lists all of its rows in its own order"
    );
}

#[test]
fn test_main_view_recent_windows_only_match_in_the_same_session() {
    let mut app = main_view_app(
        &["windows", "launchers"],
        Vec::new(),
        AddonsConfig::default(),
    );
    app.main_view.window_session = Some("/run/niri.2.sock".into());
    let mut old = recent_pick("windows", "1");
    old.session = Some("/run/niri.1.sock".into());
    let mut current = recent_pick("windows", "2");
    current.session = Some("/run/niri.2.sock".into());
    app.main_view.recent = vec![old, current];
    let window = |id: &str, title: &str| super::command_list::CommandItem {
        match_text: title.into(),
        primary: Some(super::command_list::CommandAction {
            action: DeferredAction::Command(vec!["true".into()]),
            confirm: None,
            keep_open: false,
            progress: None,
        }),
        ..super::command_list::CommandItem::info(id, title.into(), "window")
    };
    let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
        panic!("command list source");
    };
    let generation = list.generation;
    let _ = app.update(Message::MainViewLoaded(
        0,
        generation,
        Ok(vec![window("1", "Firefox"), window("2", "Terminal")]),
    ));
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(
        main_view_titles(&app),
        ["Terminal"],
        "window 1 of an earlier session is another window now"
    );
}

#[test]
fn test_main_view_selection_stays_when_recent_rows_move_to_another_source() {
    for result in [Err("no compositor".to_string()), Ok(Vec::new())] {
        let mut app = main_view_app(
            &["browser_tabs", "launchers", "windows"],
            firefox_launcher(),
            AddonsConfig::default(),
        );
        app.main_view.window_session = Some("/run/niri.sock".into());
        let mut recent: Vec<_> = (0..super::main_view::MAX_ROWS)
            .map(|id| super::recent::RecentPick {
                session: Some("/run/niri.sock".into()),
                ..recent_pick("windows", &id.to_string())
            })
            .collect();
        recent.push(recent_pick("browser_tabs", "10:1"));
        app.main_view.recent = recent;
        let super::main_view::Source::List(windows) = &mut app.main_view.sources[1] else {
            panic!("command list source");
        };
        windows.loading = true;
        let generation = windows.generation;
        load_main_view_tabs(&mut app);
        let _ = app.update(Message::SearchChanged(String::new()));
        assert!(
            main_view_titles(&app).is_empty(),
            "loading windows hold the slots"
        );
        assert_eq!(keyword_names(&app).len(), 2, "tabs and the loading windows");
        app.selected_index = 2;

        let _ = app.update(Message::MainViewLoaded(1, generation, result));
        assert_eq!(
            main_view_titles(&app),
            ["Raffi"],
            "the tab takes a free slot"
        );
        assert_eq!(keyword_names(&app), ["tabs"], "windows found none");
        assert_eq!(app.selected_index, 2, "the launcher stays selected");
    }
}

#[test]
fn test_main_view_recording_keeps_picks_of_other_instances() {
    let mut app = main_view_app(
        &["browser_tabs", "pass"],
        Vec::new(),
        AddonsConfig::default(),
    );
    let dir = std::env::temp_dir().join(format!("raffi-recent-app-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("recent.cache");
    super::recent::save(Some(&path), &[recent_pick("browser_tabs", "10:1")]);
    app.main_view.recent_path = Some(path.clone());

    app.record_recent(recent_pick("pass", "entry:mail"));
    let keys: Vec<_> = super::recent::load(Some(&path))
        .into_iter()
        .map(|pick| pick.key)
        .collect();
    assert_eq!(keys, ["entry:mail", "10:1"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_main_view_enter_on_a_recent_tab_switches_and_records_it() {
    let mut app = main_view_app(
        &["browser_tabs", "launchers"],
        firefox_launcher(),
        AddonsConfig::default(),
    );
    app.main_view.recent = vec![
        recent_pick("browser_tabs", "10:1"),
        recent_pick("browser_tabs", "10:2"),
    ];
    load_main_view_tabs(&mut app);
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(main_view_titles(&app), ["Raffi", "Mail"]);

    // Past the tabs plugin row and the first recent tab.
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::BrowserTab(super::browser_tabs::TabAction {
            tab_id: 2,
            ..
        }))
    ));
    let newest = &app.main_view.recent[0];
    assert_eq!(newest.key, "10:2");
    assert_eq!(
        newest.url.as_deref(),
        Some("https://mail.example.com/inbox")
    );
    assert_eq!(app.main_view.recent.len(), 2);
}

#[test]
fn test_main_view_ctrl_tab_focuses_each_source_then_all() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Google".into(),
        keyword: "g".into(),
        url: "https://google.com/search?q={query}".into(),
        icon: None,
        ..Default::default()
    });
    let mut app = test_app_with_fallbacks(addons, vec!["addons.web_searches.Google".into()]);
    app.configs = firefox_launcher();
    let entries = ["browser_tabs", "launchers", "system"].map(String::from);
    let _ = app.configure_main_view(Some(&entries));
    load_main_view_tabs(&mut app);
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(keyword_names(&app), ["tabs", "sys"], "detected plugins");
    assert_eq!(
        app.total_items(),
        1 + 1,
        "the plugin chips and launcher, tabs wait for a query"
    );
    assert_eq!(app.main_view_focus_label(), "All");

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view_focus_label(), "Tabs");
    assert!(app.keyword_suggestions.is_empty(), "no plugin rows");
    assert!(app.filtered_configs.is_empty(), "launchers are hidden");
    assert_eq!(app.total_items(), 2, "every tab, without a query");

    let _ = app.update(Message::SearchChanged("mail".into()));
    assert_eq!(app.main_view_focus_label(), "Tabs", "the focus stays");
    assert_eq!(app.fallback_count(), 0, "fallbacks only show in All");
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.resolve_selected_preview().unwrap().title, "Mail");

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view_focus_label(), "Launchers");
    assert_eq!(app.filtered_configs, vec![0]);
    assert_eq!(app.total_items(), 1, "no tab row");

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view_focus_label(), "System");
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view_focus_label(), "All");
    assert_eq!(app.total_items(), 1 + 1 + 1, "tab, launcher and fallback");

    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::CycleView(true));
    assert_eq!(app.main_view_focus_label(), "System", "Shift goes back");
    assert_eq!(app.selected_index, 0);

    let _ = app.update(Message::ToggleHelp);
    assert!(app
        .help_entries
        .iter()
        .any(|entry| entry.trigger == "Ctrl+Tab"));
}

#[test]
fn test_main_view_focus_lists_every_match_without_inline_rows() {
    let mut addons = AddonsConfig::default();
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Notes".into(),
        keyword: "nt".into(),
        icon: None,
        snippets: Some(Vec::new()),
        file: None,
        command: None,
        directory: None,
        args: Vec::new(),
        action: None,
        secondary_action: None,
        placeholders: false,
        restore_clipboard: false,
        ..Default::default()
    });
    let mut app = main_view_app(&["launchers", "text_snippets.Notes"], Vec::new(), addons);
    let snippets = (0..8)
        .map(|index| TextSnippet {
            name: format!("mail {index}"),
            value: format!("1+{index}"),
            keyword: None,
            icon: None,
        })
        .collect();
    let _ = app.update(Message::MainViewSnippetsLoaded(0, Ok(snippets)));
    let _ = app.update(Message::SearchChanged("mail".into()));
    assert_eq!(app.total_items(), super::main_view::MAX_ROWS);

    let _ = app.update(Message::CycleView(false));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view_focus_label(), "Notes");
    assert_eq!(app.total_items(), 8, "no cap on a focused source");
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.total_items(), 8);

    let _ = app.update(Message::SearchChanged("1+1".into()));
    assert!(app.inline_result.is_none(), "inline rows only show in All");
    let _ = app.update(Message::CycleView(false));
    assert!(app.inline_result.is_some());
}

#[test]
fn test_ctrl_tab_completes_where_the_main_view_does_not_show() {
    let mut app = test_app(snippet_addons());
    let _ = app.update(Message::SearchChanged("sn".into()));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.search_query, "sn ", "no main_view configured");

    let mut app = main_view_app(&["text_snippets.Snippets"], Vec::new(), snippet_addons());
    let _ = app.update(Message::SearchChanged("sn em".into()));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.main_view.focus, None, "keyword modes keep their list");
}

#[test]
fn test_route_query_slack_needs_enabling() {
    let mut addons = AddonsConfig::default();
    assert_eq!(route_query("slack general", &addons), QueryMode::Standard);

    addons.slack.enabled = true;
    assert_eq!(
        route_query("slack general", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Slack,
            query: "general".into()
        }
    );
    assert_eq!(keyword_suggestions("sla", &addons)[0].keyword, "slack");

    addons.slack.trigger = Some("sl".into());
    assert_eq!(
        route_query("sl search deploy", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Slack,
            query: "search deploy".into()
        }
    );
}

#[test]
fn test_slack_ctrl_tab_rewrites_the_view_word() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("slack ops".into()));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Home"));
    for (backwards, query, label) in [
        (false, "slack unread ops", "Unread"),
        (false, "slack threads ops", "Threads"),
        (false, "slack activity ops", "Activity"),
        (false, "slack people ops", "People"),
        (false, "slack actions ops", "Actions"),
        (false, "slack ops", "Home"),
        (true, "slack actions ops", "Actions"),
        (true, "slack people ops", "People"),
        (true, "slack activity ops", "Activity"),
        (true, "slack threads ops", "Threads"),
        (true, "slack unread ops", "Unread"),
        (true, "slack ops", "Home"),
    ] {
        let _ = app.update(Message::CycleView(backwards));
        assert_eq!(app.search_query, query);
        let _ = app.update(Message::SearchChanged(query.into()));
        assert_eq!(app.cycle_view_label().as_deref(), Some(label));
    }
    // A search counts as Home; an empty rest keeps the trailing space.
    let _ = app.update(Message::SearchChanged("slack search".into()));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.search_query, "slack unread ");

    // A plain alias stays; one carrying a view falls back to the trigger.
    app.addons.launcher_keywords = vec![
        launcher_keyword("sk", "slack"),
        launcher_keyword("su", "slack unread"),
    ];
    let _ = app.update(Message::SearchChanged("sk ops".into()));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.search_query, "sk unread ops");
    let _ = app.update(Message::SearchChanged("su ops".into()));
    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.search_query, "slack threads ops");
    let _ = app.update(Message::SearchChanged("slack threads ops".into()));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Threads"));

    // When a launcher keyword shadows the trigger, no typed query reaches
    // the other views, so Ctrl+Tab leaves the query alone.
    app.addons.launcher_keywords = vec![launcher_keyword("slack", "slack unread")];
    let _ = app.update(Message::SearchChanged("slack ops".into()));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Unread"));
    let task = app.update(Message::CycleView(false));
    assert_eq!(task.units(), 0);
    assert_eq!(app.search_query, "slack ops");
}

#[test]
fn test_slack_load_without_token_command_explains_setup() {
    let error = super::slack::load_home(
        crate::SlackAddonConfig {
            enabled: true,
            ..Default::default()
        },
        false,
    )
    .unwrap_err();
    assert!(error.contains("addons.slack.token_command"), "{error}");
}

#[test]
fn test_slack_rows_near_follow_the_scroll_and_the_selection() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("slack ".into()));
    let rows: Vec<_> = (0..200)
        .map(|n| {
            let id = format!("slack:user:U{n}");
            super::command_list::CommandItem::info(&id, id.clone(), "icon")
        })
        .collect();
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));
    let selected = app.selected_index - app.command_list_offset();
    let _ = app.update(Message::ListScrolled(0.5));
    // Halfway down, the 20 rows shown start at row 90.
    assert!((90..110).all(|row| app.slack_row_near(row, 20)));
    assert!(app.slack_row_near(selected, 20));
    assert!(!app.slack_row_near(150, 20));
    // Leaving Slack stops the batch downloading for it.
    app.slack.images_inflight = Some(app.slack.images_request);
    let before = super::slack::image_generation();
    let _ = app.update(Message::SearchChanged("".into()));
    assert_ne!(super::slack::image_generation(), before);
    assert_eq!(app.slack.images_inflight, None);
}

#[test]
fn test_slack_images_apply_once_their_batch_lands_and_survive_reloads() {
    use super::slack::ImagesOutcome;
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("slack ".into()));
    let rows = || {
        ["slack:user:U1", "slack:user:U2"]
            .map(|id| super::command_list::CommandItem::info(id, id.into(), "icon"))
            .to_vec()
    };
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows())));
    let session = app.command_list.session;
    let epoch = app.slack.epoch;
    let icon = |app: &LauncherApp, id: &str| {
        let row = app.command_list.position(id).unwrap();
        app.command_list.item(row).unwrap().icon.clone()
    };
    let found = |id: &str| ImagesOutcome::Attempted(vec![(id.into(), format!("/a/{id}.png"))]);

    // A batch dropped by a newer one neither shows nor releases it.
    app.slack.images_inflight = Some(5);
    let _ = app.update(Message::SlackImagesLoaded(
        session,
        epoch,
        4,
        vec!["U1".into()],
        found("U1"),
    ));
    assert_eq!(icon(&app, "slack:user:U1"), None);
    assert_eq!(app.slack.images_inflight, Some(5));
    let _ = app.update(Message::SlackImagesLoaded(
        session,
        epoch,
        5,
        vec!["U1".into(), "U9".into()],
        found("U1"),
    ));
    assert_eq!(icon(&app, "slack:user:U1").as_deref(), Some("/a/U1.png"));
    assert_eq!(app.slack.images_inflight, None);
    assert!(
        app.slack.images_tried.contains("U9"),
        "failures are not retried"
    );

    // A preview's avatar is kept too, and both survive the rows reloading.
    let request = app.slack.preview_request;
    let _ = app.update(Message::SlackPreviewLoaded(
        session,
        "slack:user:U2".into(),
        request,
        Some((Vec::new(), Some("/a/U2.png".into()))),
    ));
    app.set_command_list_items(rows());
    assert_eq!(icon(&app, "slack:user:U1").as_deref(), Some("/a/U1.png"));
    assert_eq!(icon(&app, "slack:user:U2").as_deref(), Some("/a/U2.png"));

    // Not connected yet: nothing counts as tried, and rows loading again
    // lets the next batch try.
    app.slack.images_inflight = Some(6);
    let _ = app.update(Message::SlackImagesLoaded(
        session,
        epoch,
        6,
        vec!["U3".into()],
        ImagesOutcome::Skipped,
    ));
    assert!(app.slack.images_blocked);
    assert!(!app.slack.images_tried.contains("U3"));
    app.set_command_list_items(rows());
    assert!(!app.slack.images_blocked);

    // Another account drops the batch in flight and the pictures shown.
    app.slack.images_inflight = Some(7);
    app.sync_slack_account_to(epoch + 1);
    assert_eq!(app.slack.images_inflight, None);
    assert!(app.slack.images_tried.is_empty());
    let _ = app.update(Message::SlackImagesLoaded(
        session,
        epoch,
        7,
        vec!["U2".into()],
        found("U2"),
    ));
    app.set_command_list_items(rows());
    assert_eq!(icon(&app, "slack:user:U1"), None);
}

#[test]
fn test_slack_people_show_full_names_and_logins() {
    use super::slack_api::{Directory, User};

    let users = [
        ("alice", "Alice Adams", "alice", "Alice Adams (alice)"),
        ("alice", "", "Alice", "Alice (alice)"),
        ("alice", "", "alice", "alice"),
        ("alice", "", "", "alice"),
        ("", "Alice Adams", "", "Alice Adams"),
        ("", "", "", "U1"),
    ];
    for (login, real_name, display_name, expected) in users {
        let directory = Directory::new(
            Default::default(),
            vec![User {
                id: "U1".into(),
                name: login.into(),
                real_name: real_name.into(),
                display_name: display_name.into(),
                ..User::default()
            }],
            Vec::new(),
            0,
        );
        let rows = super::slack::home_rows(
            &directory,
            &super::slack::Live::default(),
            None,
            crate::SlackOpenIn::Desktop,
            chrono::Local::now(),
        );
        let person = rows
            .iter()
            .find(|row| row.identity == "slack:user:U1")
            .unwrap();
        assert_eq!(person.title, expected);
    }
}

#[test]
fn test_slack_status_fill_and_debounced_search() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;

    let task = app.update(Message::SearchChanged("slack ".into()));
    assert!(task.units() > 0, "the home list loads");
    assert_eq!(app.command_list.status(), Some("Loading Slack..."));
    let rows = super::slack::home_rows(
        &super::slack_api::Directory::default(),
        &super::slack::Live::default(),
        None,
        crate::SlackOpenIn::Desktop,
        chrono::Local::now(),
    );
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));

    // Picking "Set status" puts `status ` after the keyword.
    let _ = app.update(Message::SearchChanged("slack set status".into()));
    assert_eq!(command_list_titles(&app)[0], "Set status");
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "slack status ");
    assert!(deferred_action(&app).is_none());
    assert_eq!(command_list_titles(&app)[0], "Type the status to set");
    assert_eq!(
        app.command_list.generation, generation,
        "home is not reloaded"
    );

    let _ = app.update(Message::SearchChanged(
        "slack status Lunch :pizza: 1h".into(),
    ));
    assert_eq!(command_list_titles(&app)[0], "Set status to :pizza: Lunch");

    // A search waits for two characters, then for the typing to settle.
    let _ = app.update(Message::SearchChanged("slack search d".into()));
    assert_eq!(
        command_list_titles(&app),
        vec!["Type the words to search for"]
    );
    let task = app.update(Message::SearchChanged("slack search deploy".into()));
    assert!(task.units() > 0, "the search is scheduled");
    let generation = app.command_list.generation;
    let session = app.command_list.session;
    let _ = app.update(Message::SlackSearchDue(session, "dep".into()));
    assert_eq!(
        app.command_list.generation, generation,
        "stale terms are ignored"
    );
    let _ = app.update(Message::SlackSearchDue(session, "deploy".into()));
    assert_ne!(app.command_list.generation, generation, "the search runs");
}

#[test]
fn test_slack_previews_of_another_account_are_dropped() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("slack unread ".into()));
    let identity = "slack:unread:C1".to_string();
    let rows = vec![super::command_list::CommandItem::info(
        &identity,
        "#general".into(),
        "icon",
    )];
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));
    let _ = app.ensure_slack_preview();
    assert_eq!(app.command_list.preview_inflight.as_ref(), Some(&identity));
    let old_request = app.slack.preview_request;
    let session = app.command_list.session;
    let text = |content: &str| super::preview::PreviewBody::Text {
        heading: None,
        content: content.into(),
        truncated: false,
    };
    app.command_list
        .previews
        .insert(identity.clone(), vec![text("account A")]);

    // Slack says the token is someone else's.
    app.sync_slack_account_to(app.slack.epoch + 1);
    assert!(app.command_list.previews.is_empty());
    assert_eq!(app.command_list.preview_inflight, None);
    let _ = app.ensure_slack_preview();
    let request = app.slack.preview_request;
    assert_ne!(request, old_request);

    let _ = app.update(Message::SlackPreviewLoaded(
        session,
        identity.clone(),
        old_request,
        Some((vec![text("late A")], None)),
    ));
    assert!(
        app.command_list.previews.is_empty(),
        "the old one is dropped"
    );
    assert_eq!(
        app.command_list.preview_inflight.as_ref(),
        Some(&identity),
        "and does not release the new request"
    );
    let _ = app.update(Message::SlackPreviewLoaded(
        session,
        identity.clone(),
        request,
        Some((vec![text("account B")], None)),
    ));
    assert_eq!(app.command_list.previews[&identity], [text("account B")]);
    assert_eq!(app.command_list.preview_inflight, None);
}

#[test]
fn test_slack_unread_enter_produces_open_unread_action() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    addons.slack.close_on_action = Some(true);
    let mut app = test_app(addons);
    app.history.max_items = 0;

    let _ = app.update(Message::SearchChanged("slack unread ".into()));
    let unread = [super::slack::Unread {
        channel: "C1".into(),
        last_read: "1.0".into(),
        latest: "5.0".into(),
        mentions: 1,
        count: Some(3),
        muted: false,
    }];
    let mut dir = super::slack_api::Directory::default();
    dir.workspace.team_id = "T1".into();
    let rows = super::slack::unread_rows(
        &unread,
        &dir,
        crate::SlackOpenIn::Desktop,
        chrono::Local::now(),
    );
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));

    app.selected_index = app.command_list_offset();
    let _ = app.update(Message::Submit);
    let Some(super::actions::DeferredAction::Slack(action)) = deferred_action(&app) else {
        panic!("expected a Slack action");
    };
    assert_eq!(
        action,
        super::slack::SlackAction::OpenUnread {
            url: "slack://channel?team=T1&id=C1".into(),
            channel: "C1".into(),
            ts: "5.0".into(),
        }
    );
}

#[test]
fn test_ctrl_r_refreshes_slack_view_and_preview() {
    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;

    let _ = app.update(Message::SearchChanged("slack unread ".into()));
    let identity = "slack:unread:C1:1.0".to_string();
    let rows = vec![super::command_list::CommandItem::info(
        &identity,
        "#general".into(),
        "icon",
    )];
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));
    let text = super::preview::PreviewBody::Text {
        heading: None,
        content: "cached preview".into(),
        truncated: false,
    };
    app.command_list
        .previews
        .insert(identity.clone(), vec![text.clone()]);

    let before_generation = app.command_list.generation;
    let before_preview_req = app.slack.preview_request;
    let task = app.update(Message::ClipboardRefresh);
    assert!(task.units() > 0, "Ctrl+R refreshes Slack");
    assert_ne!(app.command_list.generation, before_generation);
    assert_eq!(
        command_list_titles(&app),
        vec!["#general"],
        "existing rows stay visible while refreshing"
    );
    assert_ne!(
        app.slack.preview_request, before_preview_req,
        "selected preview is refreshed too"
    );
    assert_eq!(app.command_list.preview_inflight.as_ref(), Some(&identity));
}

#[test]
fn test_ctrl_r_refreshes_command_list_and_file_browser() {
    let mut addons = AddonsConfig::default();
    addons.kill.enabled = true;
    addons.file_browser.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;

    let _ = app.update(Message::SearchChanged("kill ".into()));
    let generation = app.command_list.generation;
    let rows = vec![super::command_list::CommandItem::info(
        "pid:1",
        "init".into(),
        "icon",
    )];
    let _ = app.update(Message::CommandListLoaded(generation, Ok(rows)));
    let task = app.update(Message::ClipboardRefresh);
    assert!(task.units() > 0, "Ctrl+R reloads command list");
    assert_ne!(app.command_list.generation, generation);
    assert_eq!(command_list_titles(&app), vec!["init"]);

    let dir = std::env::temp_dir().join(format!("raffi-ctrl-r-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("first.txt"), "a").unwrap();
    let query = format!("{}/", dir.display());
    let _ = app.update(Message::SearchChanged(query));
    assert_eq!(app.file_browser.entries.len(), 1);
    std::fs::write(dir.join("second.txt"), "b").unwrap();
    let _ = app.update(Message::ClipboardRefresh);
    assert_eq!(app.file_browser.entries.len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_command_list_lists_dimmed_rows_last_until_a_filter_is_typed() {
    use super::command_list::{filter_items_into, CommandItem, MatchMode};
    let item = |id: &str, dimmed: bool| CommandItem {
        match_text: id.into(),
        dimmed,
        ..CommandItem::info(id, id.into(), "icon")
    };
    let items = [
        item("ops-muted", true),
        item("dev", false),
        item("ops", false),
    ];
    let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
    let usage = std::collections::HashMap::from([("ops-muted".to_string(), 5)]);
    let mut out = Vec::new();
    filter_items_into(
        &items,
        "",
        &matcher,
        &usage,
        false,
        MatchMode::Fuzzy,
        &mut out,
    );
    assert_eq!(out, [1, 2, 0], "muted last even when used most");
    filter_items_into(
        &items,
        "muted",
        &matcher,
        &usage,
        false,
        MatchMode::Fuzzy,
        &mut out,
    );
    assert_eq!(out, [0], "filtering still finds muted rows");
}

#[test]
fn test_command_list_hides_typed_only_rows_until_the_query_matches() {
    use super::command_list::{filter_items_into, CommandItem, MatchMode};
    let item = |id: &str, typed_only: bool| CommandItem {
        match_text: id.into(),
        typed_only,
        ..CommandItem::info(id, id.into(), "icon")
    };
    let items = [item("snooze", true), item("ops", false)];
    let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
    let usage = std::collections::HashMap::new();
    let mut out = Vec::new();
    filter_items_into(
        &items,
        "",
        &matcher,
        &usage,
        false,
        MatchMode::Fuzzy,
        &mut out,
    );
    assert_eq!(out, [1]);
    filter_items_into(
        &items,
        "snoo",
        &matcher,
        &usage,
        false,
        MatchMode::Fuzzy,
        &mut out,
    );
    assert_eq!(out, [0]);
}

#[test]
fn test_command_list_sort_priority_precedes_usage_and_keeps_priority_order() {
    use super::command_list::{filter_items_into, CommandItem, MatchMode};
    let item = |id: &str, sort_priority: u8| CommandItem {
        sort_priority,
        ..CommandItem::info(id, id.into(), "icon")
    };
    let items = [
        item("unread-new", 1),
        item("popular", 0),
        item("actions", 2),
        item("unread-old", 1),
    ];
    let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
    let usage = std::collections::HashMap::from([
        ("popular".to_string(), 100),
        ("unread-old".to_string(), 50),
    ]);
    let mut out = Vec::new();
    filter_items_into(
        &items,
        "",
        &matcher,
        &usage,
        false,
        MatchMode::Fuzzy,
        &mut out,
    );
    assert_eq!(out, [2, 0, 3, 1]);
}

#[test]
fn test_launcher_keep_open_respects_general_and_launcher_override() {
    let mut app = test_app(AddonsConfig::default());
    app.configs = vec![
        RaffiConfig {
            description: Some("True Launcher".into()),
            binary: Some("true".into()),
            ..Default::default()
        },
        RaffiConfig {
            description: Some("Closing Launcher".into()),
            binary: Some("true".into()),
            close_on_action: Some(true),
            ..Default::default()
        },
    ];
    app.history.max_items = 0;
    app.close_on_action = Some(false);
    let _ = app.update(Message::SearchChanged("True".into()));
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0, "general.close_on_action=false keeps open");
    assert_eq!(app.search_query, "True", "search query preserved");
    assert!(app.selected_item.lock().unwrap().is_none());

    let _ = app.update(Message::SearchChanged("Closing".into()));
    let task = app.update(Message::Submit);
    assert!(
        task.units() > 0,
        "launcher close_on_action=true overrides general"
    );
    assert_eq!(
        app.selected_item.lock().unwrap().as_deref(),
        Some("Closing Launcher")
    );
}

#[test]
fn test_launcher_raises_its_open_window_unless_ctrl() {
    let mut app = test_app(AddonsConfig::default());
    app.configs = vec![RaffiConfig {
        description: Some("Firefox".into()),
        binary: Some("firefox".into()),
        raise: Some(true),
        ..Default::default()
    }];
    app.history.max_items = 0;
    // The lookup task is never run here, so no compositor is reached.
    app.view.raise_via = Some(Compositor::Hyprland);
    app.view.show_launcher_preview = true;
    let _ = app.update(Message::SearchChanged("Firefox".into()));
    let keys: Vec<(String, String)> = app.resolve_selected_preview().unwrap().metadata;
    assert!(keys.contains(&(
        "Enter".to_string(),
        "Focus its open window, or launch".to_string()
    )));
    assert!(keys.contains(&("Ctrl+Enter".to_string(), "Launch a new one".to_string())));

    let task = app.update(Message::Submit);
    assert!(task.units() > 0);
    assert!(
        app.selected_item.lock().unwrap().is_none(),
        "Enter looks for a window before launching"
    );

    // An open window is focused once Raffi has closed.
    let _ = app.update(Message::LauncherRaiseChecked(
        0,
        Some(DeferredAction::Command(vec!["true".into()])),
    ));
    assert!(matches!(
        *app.deferred_action.lock().unwrap(),
        Some(DeferredAction::Command(_))
    ));
    assert!(app.selected_item.lock().unwrap().is_none());

    // Without one, the launcher starts as usual.
    *app.deferred_action.lock().unwrap() = None;
    let _ = app.update(Message::LauncherRaiseChecked(0, None));
    assert_eq!(
        app.selected_item.lock().unwrap().as_deref(),
        Some("Firefox")
    );
    assert!(app.deferred_action.lock().unwrap().is_none());

    // Ctrl+Enter starts a new one without looking.
    *app.selected_item.lock().unwrap() = None;
    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    let _ = app.update(Message::Submit);
    assert_eq!(
        app.selected_item.lock().unwrap().as_deref(),
        Some("Firefox")
    );
}

#[test]
fn test_addon_close_on_action_precedence() {
    let mut addons = AddonsConfig::default();
    addons.calculator.close_on_action = Some(true);
    addons.emoji.action = Some("true".into());
    addons.emoji.close_on_action = Some(false);

    let mut app = test_app(addons);
    app.history.max_items = 0;
    app.close_on_action = Some(false);

    // Inline units inherits general.close_on_action = false -> stays open after copy
    let _ = app.update(Message::SearchChanged("10 km in miles".into()));
    assert!(app.inline_result.is_some());
    let task = app.update(Message::InlineCopied(app.inline_generation, Ok(())));
    assert_eq!(task.units(), 0, "units inherits general keep-open");
    assert_eq!(app.search_query, "10 km in miles");

    // Calculator overrides close_on_action = true -> closes after copy
    let _ = app.update(Message::SearchChanged("2 + 2".into()));
    assert!(app.inline_result.is_some());
    let task = app.update(Message::InlineCopied(app.inline_generation, Ok(())));
    assert!(task.units() > 0, "calculator override closes window");

    // Emoji overrides close_on_action = false even when general is true
    app.close_on_action = Some(true);
    let _ = app.update(Message::SearchChanged("emoji party".into()));
    let _ = app.update(Message::EmojiDataLoaded(vec![EmojiEntry {
        value: "🎉".into(),
        name: "party popper".into(),
    }]));
    assert_eq!(app.emoji.filtered.len(), 1);
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0, "emoji close_on_action=false keeps open");
    assert_eq!(app.search_query, "emoji party");

    // Emoji insert action must still close even when close_on_action is false
    app.emoji.action = Some("insert".into());
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "insert action always closes to type");
}

#[test]
fn test_file_browser_insert_forces_close_when_kept_open() {
    use super::state::FileBrowserEntry;
    let mut addons = AddonsConfig::default();
    addons.file_browser.action = Some("true".into());
    addons.file_browser.secondary_action = Some("insert".into());
    addons.file_browser.close_on_action = Some(false);

    let mut app = test_app(addons);
    app.history.max_items = 0;
    app.close_on_action = Some(false);
    app.file_browser.active = true;
    app.file_browser.entries = vec![FileBrowserEntry {
        name: "note.txt".into(),
        full_path: "/tmp/note.txt".into(),
        is_dir: false,
    }];
    app.selected_index = 0;

    // Primary non-insert action stays open
    let task = app.update(Message::Submit);
    assert_eq!(task.units(), 0, "non-insert file browser action stays open");

    // Secondary insert action (Ctrl+Enter) forces close
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let task = app.update(Message::Submit);
    assert!(
        task.units() > 0,
        "secondary insert action forces Raffi to close"
    );

    // Primary insert action also forces close
    app.view.current_modifiers = iced::keyboard::Modifiers::empty();
    app.addons.file_browser.action = Some("insert".into());
    let task = app.update(Message::Submit);
    assert!(
        task.units() > 0,
        "primary insert action forces Raffi to close"
    );
}

#[test]
fn test_main_view_keep_open_preserves_query_and_selection() {
    let mut addons = AddonsConfig::default();
    addons.browser_tabs.close_on_action = Some(false);
    addons.text_snippets.push(TextSnippetSourceConfig {
        name: "Notes".into(),
        keyword: "sn".into(),
        snippets: Some(Vec::new()),
        action: Some("true".into()),
        close_on_action: Some(false),
        placeholders: true,
        ..Default::default()
    });
    addons.script_filters.push(ScriptFilterConfig {
        name: "Issues".into(),
        command: "issues".into(),
        keyword: "is".into(),
        action: Some("true".into()),
        close_on_action: Some(false),
        ..Default::default()
    });

    let mut app = main_view_app(
        &[
            "browser_tabs",
            "text_snippets.Notes",
            "script_filters.Issues",
        ],
        Vec::new(),
        addons,
    );
    load_main_view_tabs(&mut app);
    if let super::main_view::Source::List(list) = &mut app.main_view.sources[0] {
        for item in &mut list.items {
            if let Some(primary) = &mut item.primary {
                primary.keep_open = true;
            }
        }
    }
    let _ = app.update(Message::MainViewSnippetsLoaded(
        1,
        Ok(vec![
            TextSnippet {
                name: "mail signature A".into(),
                value: "Regards A".into(),
                keyword: None,
                icon: None,
            },
            TextSnippet {
                name: "mail signature B".into(),
                value: "Regards B".into(),
                keyword: None,
                icon: None,
            },
        ]),
    ));
    let _ = app.update(Message::SearchChanged("mail".into()));
    let run = script_filter_generation(&app, 2);
    let _ = app.update(Message::MainViewScriptFilter(
        2,
        run,
        Some(Ok(ScriptFilterResult {
            default_icon: None,
            items: vec![ScriptFilterItem {
                title: "mail bug".into(),
                valid: true,
                subtitle: None,
                arg: Some("123".into()),
                indent: 0,
                icon: None,
                preview: None,
                quicklookurl: None,
                variables: Default::default(),
                text: None,
            }],
        })),
    ));
    assert_eq!(app.total_items(), 4);

    // 1. Activating a kept-open browser tab row in main view keeps query "mail" and main view active,
    //    AND script filter rows remain present immediately without disappearing!
    app.selected_index = 0;
    let tab_session = app.command_list.session.wrapping_add(1);
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "mail");
    assert!(app.main_view.active);
    assert_eq!(app.total_items(), 4, "script filter rows preserved");
    assert_eq!(app.selected_index, 0);
    assert!(app.in_flight.is_pending("browser_tabs", "10:2"));

    // 2. Activating the SECOND snippet ("mail signature B" at index 2) records usage so B ranks
    //    ahead of A ([B, A]), and restoration follows B's identity to its new row (index 1)!
    app.selected_index = 2;
    let task = app.update(Message::Submit);
    assert!(task.units() > 0, "starts async placeholder expansion");
    let session = app.text_snippets.session;
    let task = app.update(Message::TextSnippetExpanded(
        session,
        Ok(Expanded {
            text: "Regards B".into(),
            cursor_moves: None,
        }),
    ));
    assert_eq!(task.units(), 0, "stays open and restores main view");
    assert_eq!(app.search_query, "mail");
    assert!(app.main_view.active);
    assert_eq!(app.total_items(), 4, "script filter rows still preserved");
    assert_eq!(
        app.selected_index, 1,
        "selection follows snippet B to its new MRU-promoted row"
    );
    assert_eq!(app.selected_main_view(), Some((1, 0)));
    if let super::main_view::Source::Snippets {
        items, filtered, ..
    } = &app.main_view.sources[1]
    {
        assert_eq!(items[filtered[0]].name, "mail signature B");
    } else {
        panic!("expected snippet source");
    }

    // 3. Activating a kept-open script filter row in main view preserves query, rows, and selection
    app.selected_index = 3;
    let _ = app.update(Message::Submit);
    assert_eq!(app.search_query, "mail");
    assert!(app.main_view.active);
    assert_eq!(
        app.total_items(),
        4,
        "script filter rows preserved on SF submit"
    );
    assert_eq!(app.selected_index, 3);

    // 4. If the in-flight browser tab action fails while in main view, the error is surfaced visibly
    let _ = app.update(Message::CommandListActionDone(
        CommandListAddon::BrowserTabs,
        tab_session,
        "10:2".into(),
        Err("browser socket closed".into()),
    ));
    assert_eq!(app.command_list.addon, Some(CommandListAddon::BrowserTabs));
    assert!(
        command_list_titles(&app)[0].contains("browser socket closed"),
        "action error from main view surfaced in command list"
    );
}

#[test]
fn test_vault_keep_open_tracks_copy_completion_and_errors() {
    use super::secret_io::SecretError;
    use super::vault::{OpKind, Outcome};

    let mut app = pass_app();
    app.addons.pass.close_on_action = Some(false);
    select_identity(&mut app, "pass:entry:web/github");

    let _ = app.update(Message::Submit);
    let fetch_gen = app.vault.generation;
    let task = app.update(Message::VaultDone(fetch_gen, vault_report(ready_job())));
    assert!(task.units() > 0, "spawns tracked clipboard copy task");
    assert!(
        app.vault.busy(),
        "vault stays busy while clipboard copy is running"
    );
    assert_eq!(
        app.vault.running.as_ref().map(|op| op.kind.clone()),
        Some(OpKind::Copy("web/github".into()))
    );
    assert_eq!(app.command_list.status(), Some("Copying..."));

    // Overlapping selection while copy is running is ignored
    let copy_gen = app.vault.generation;
    assert_eq!(app.update(Message::Submit).units(), 0);

    // Copy failure is reported in the command list error row
    let task = app.update(Message::VaultDone(
        copy_gen,
        vault_report(Outcome::Failed(SecretError::Tool(
            "copy not confirmed".into(),
        ))),
    ));
    assert_eq!(task.units(), 0);
    assert!(!app.vault.busy());
    assert_eq!(command_list_titles(&app)[0], "copy not confirmed");
}

#[test]
fn test_slack_mrkdwn_parsing_and_chat_preview_header() {
    use super::preview::{
        parse_mrkdwn_blocks, parse_mrkdwn_spans, ChatEntry, ChatMessage, ChatMessageGroup,
        MrkdwnBlock, MrkdwnSpan, MrkdwnStyle, PreviewBody,
    };

    let blocks = parse_mrkdwn_blocks("Hello world\n> quoted line\n```cargo test\n```\nafter");
    assert_eq!(
        blocks,
        vec![
            MrkdwnBlock::Paragraph("Hello world".into()),
            MrkdwnBlock::Quote("quoted line".into()),
            MrkdwnBlock::Code("cargo test".into()),
            MrkdwnBlock::Paragraph("after".into()),
        ]
    );

    let spans = parse_mrkdwn_spans(
        "Hey @alice check #ops: *bold*, _italic_, ~old~, `fn_name()`, snake_case_var and https://example.com/a.",
    );
    assert_eq!(
        spans,
        vec![
            MrkdwnSpan {
                text: "Hey ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "@alice".into(),
                style: MrkdwnStyle::Mention,
            },
            MrkdwnSpan {
                text: " check ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "#ops".into(),
                style: MrkdwnStyle::Mention,
            },
            MrkdwnSpan {
                text: ": ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "bold".into(),
                style: MrkdwnStyle::Bold,
            },
            MrkdwnSpan {
                text: ", ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "italic".into(),
                style: MrkdwnStyle::Italic,
            },
            MrkdwnSpan {
                text: ", ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "old".into(),
                style: MrkdwnStyle::Strike,
            },
            MrkdwnSpan {
                text: ", ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "fn_name()".into(),
                style: MrkdwnStyle::Code,
            },
            MrkdwnSpan {
                text: ", snake_case_var and ".into(),
                style: MrkdwnStyle::Plain,
            },
            MrkdwnSpan {
                text: "https://example.com/a".into(),
                style: MrkdwnStyle::Link,
            },
            MrkdwnSpan {
                text: ".".into(),
                style: MrkdwnStyle::Plain,
            },
        ]
    );

    let mut addons = AddonsConfig::default();
    addons.slack.enabled = true;
    let mut app = test_app(addons);
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged("slack ".into()));
    let mut row =
        super::command_list::CommandItem::info("slack:conv:C1", "#general".into(), "slack");
    row.subtitle = "Company-wide announcements".into();
    row.badge = Some("Channel".into());
    let generation = app.command_list.generation;
    let _ = app.update(Message::CommandListLoaded(generation, Ok(vec![row])));
    app.command_list.previews.insert(
        "slack:conv:C1".into(),
        vec![PreviewBody::Chat {
            heading: Some("Recent messages".into()),
            entries: vec![
                ChatEntry::DayDivider("Today".into()),
                ChatEntry::Messages(ChatMessageGroup {
                    author: "Alice".into(),
                    is_self: true,
                    avatar: None,
                    time: "14:22".into(),
                    messages: vec![ChatMessage {
                        time: None,
                        text: "Ship it 🚀 *now*".into(),
                        reactions: vec![("👍".into(), 2)],
                        replies: 1,
                        highlighted: true,
                        truncated: false,
                        desktop_url: Some(
                            "slack://channel?team=T1&id=C1&message=1.0&thread_ts=1.0".into(),
                        ),
                        browser_url: Some(
                            "https://acme.slack.com/archives/C1/p10?thread_ts=1.0&cid=C1".into(),
                        ),
                    }],
                }),
            ],
        }],
    );
    app.view.show_preview = true;
    let preview = app
        .resolve_selected_preview()
        .expect("Slack chat preview built");
    assert_eq!(preview.title, "#general");
    assert_eq!(
        preview.subtitle.as_deref(),
        Some("Company-wide announcements")
    );
    assert_eq!(preview.badge.as_deref(), Some("Channel"));
    let _ = app.view();

    // Clicking a thread in the preview opens the thread URL and exits by default
    let task = app.update(Message::SlackPreviewOpen {
        desktop_url: Some("slack://channel?team=T1&id=C1&message=1.0&thread_ts=1.0".into()),
        browser_url: Some("https://acme.slack.com/archives/C1/p10?thread_ts=1.0&cid=C1".into()),
    });
    assert!(task.units() > 0);
    let Some(super::actions::DeferredAction::Slack(action)) = deferred_action(&app) else {
        panic!("expected a Slack action");
    };
    assert_eq!(
        action,
        super::slack::SlackAction::Open(
            "slack://channel?team=T1&id=C1&message=1.0&thread_ts=1.0".into()
        )
    );

    // Ctrl+Click still opens with open_in: there is no "other app" any more
    app.in_flight = Default::default();
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let task = app.update(Message::SlackPreviewOpen {
        desktop_url: Some("slack://channel?team=T1&id=C1&message=1.0&thread_ts=1.0".into()),
        browser_url: Some("https://acme.slack.com/archives/C1/p10?thread_ts=1.0&cid=C1".into()),
    });
    assert!(task.units() > 0);
    let Some(super::actions::DeferredAction::Slack(action)) = deferred_action(&app) else {
        panic!("expected a Slack action");
    };
    assert_eq!(
        action,
        super::slack::SlackAction::Open(
            "slack://channel?team=T1&id=C1&message=1.0&thread_ts=1.0".into()
        )
    );
}

fn bookmarks_app(query: &str) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the bookmarks load");
    assert_eq!(app.command_list.status(), Some("Loading bookmarks..."));
    let generation = app.command_list.generation;
    let _ = app.update(Message::BookmarksLoaded(
        generation,
        Some(Arc::new(super::bookmarks::fixture_library())),
    ));
    app
}

#[test]
fn test_route_query_bookmarks_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("bm Firefox/Work/ru", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::Bookmarks,
            query: "Firefox/Work/ru".into()
        }
    );
    assert!(suggested_keywords("bookmarks", &addons).contains(&"bm".to_string()));

    addons.bookmarks.enabled = false;
    assert_eq!(route_query("bm rust", &addons), QueryMode::Standard);
}

#[test]
fn test_bookmarks_enter_and_tab_open_folders_and_backspace_goes_up() {
    let mut app = bookmarks_app("bm ");
    assert_eq!(command_list_titles(&app), ["Firefox", "Brave"]);

    let _ = app.update(Message::CommandListSelected(0));
    assert_eq!(app.search_query, "bm Firefox/");
    assert_eq!(
        command_list_titles(&app),
        ["Bookmarks Toolbar", "Other Bookmarks"]
    );

    app.selected_index = app.command_list_offset();
    let _ = app.update(Message::TabComplete);
    assert_eq!(app.search_query, "bm Firefox/Bookmarks Toolbar/");
    assert_eq!(command_list_titles(&app), ["Work", "Mail"]);

    // Backspace on the trailing `/` goes up and selects the folder left.
    let _ = app.update(Message::SearchChanged(
        "bm Firefox/Bookmarks Toolbar".into(),
    ));
    assert_eq!(app.search_query, "bm Firefox/");
    assert_eq!(app.selected_index, app.command_list_offset());

    let _ = app.update(Message::CommandListSelected(1));
    assert_eq!(app.search_query, "bm Firefox/Other Bookmarks/");
    let _ = app.update(Message::SearchChanged("bm Firefox/Other Bookmarks".into()));
    assert_eq!(app.search_query, "bm Firefox/");
    assert_eq!(app.selected_index, app.command_list_offset() + 1);

    // A filter after the path searches below it; Enter opens the link.
    let _ = app.update(Message::SearchChanged("bm firefox/rust docs".into()));
    assert_eq!(command_list_titles(&app), ["Rust docs"]);
    let _ = app.update(Message::CommandListSelected(0));
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv)) if argv == ["firefox", "https://doc.rust-lang.org/std/"]
    ));
}

#[test]
fn test_bookmarks_ctrl_tab_shows_each_source() {
    let mut app = bookmarks_app("bm Firefox/");
    assert_eq!(app.cycle_view_label().as_deref(), Some("All bookmarks"));

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Firefox"));
    assert_eq!(app.search_query, "bm ");
    assert_eq!(
        command_list_titles(&app),
        ["Bookmarks Toolbar", "Other Bookmarks"]
    );

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Brave"));
    assert_eq!(command_list_titles(&app), ["Bookmarks bar"]);

    let _ = app.update(Message::CycleView(false));
    assert_eq!(app.cycle_view_label().as_deref(), Some("All bookmarks"));
    assert_eq!(command_list_titles(&app), ["Firefox", "Brave"]);
}

#[test]
fn test_bookmarks_search_prompt_asks_for_the_term() {
    let mut app = bookmarks_app("bm nix code");
    assert_eq!(command_list_titles(&app), ["Nix code search"]);
    let row = &app.command_list.items[app.command_list.filtered[0]];
    assert_eq!(row.badge.as_deref(), Some("Search"));

    // Ctrl+Enter on the list row copies the link as saved.
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let _ = app.update(Message::CommandListSelected(0));
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Copy(url)) if url == "https://github.com/search?q=%22%s%22+path%3A*.nix"
    ));
    app.view.current_modifiers = iced::keyboard::Modifiers::empty();
    *app.deferred_action.lock().unwrap() = None;
    let finish = |app: &mut LauncherApp| {
        let name = CommandListAddon::Bookmarks.name();
        app.in_flight
            .finish(name, "bookmark:firefox:abc.default:o2");
    };
    finish(&mut app);

    let _ = app.update(Message::CommandListSelected(0));
    assert!(app.bookmarks.prompt.is_some());
    assert_eq!(app.cycle_view_label(), None, "Ctrl+Tab does nothing here");
    assert_eq!(
        command_list_titles(&app),
        ["Type what to search Nix code search for"]
    );
    // Enter without a term does nothing.
    let _ = app.update(Message::Submit);
    assert!(deferred_action(&app).is_none());
    assert!(app.bookmarks.prompt.is_some());

    let _ = app.update(Message::SearchChanged("mk shell".into()));
    assert_eq!(
        command_list_titles(&app),
        ["Search Nix code search for 'mk shell'"]
    );
    let url = "https://github.com/search?q=%22mk%20shell%22+path%3A*.nix";
    app.view.current_modifiers = iced::keyboard::Modifiers::CTRL;
    let _ = app.update(Message::Submit);
    assert!(matches!(deferred_action(&app), Some(DeferredAction::Copy(copied)) if copied == url));
    app.view.current_modifiers = iced::keyboard::Modifiers::empty();
    finish(&mut app);
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv)) if argv == ["firefox", url]
    ));
}

#[test]
fn test_bookmarks_search_prompt_leaves_on_escape_and_backspace() {
    let mut app = bookmarks_app("bm Firefox/Other Bookmarks/");
    assert_eq!(command_list_titles(&app), ["Recipes", "Nix code search"]);
    let _ = app.update(Message::CommandListSelected(1));
    let _ = app.update(Message::SearchChanged("x".into()));
    let _ = app.update(Message::Cancel);
    assert!(app.bookmarks.prompt.is_none());
    assert_eq!(app.search_query, "bm Firefox/Other Bookmarks/");
    assert_eq!(app.selected_index, app.command_list_offset() + 1);

    let _ = app.update(Message::TabComplete);
    let _ = app.update(Message::SearchChanged("x".into()));
    let _ = app.update(Message::SearchChanged(String::new()));
    assert!(app.bookmarks.prompt.is_some(), "an emptied term still asks");
    let _ = app.update(Message::SearchChanged(String::new()));
    assert!(app.bookmarks.prompt.is_none());
    assert_eq!(app.search_query, "bm Firefox/Other Bookmarks/");
    assert_eq!(app.selected_index, app.command_list_offset() + 1);
}

#[test]
fn test_bookmarks_keyword_opens_the_search_prompt() {
    let mut app = bookmarks_app("bm ");
    let _ = app.update(Message::SearchChanged("bm nixsearch flake".into()));
    let prompt = app.bookmarks.prompt.as_ref().expect("the prompt");
    assert_eq!(prompt.term, "flake");
    assert_eq!(
        command_list_titles(&app),
        ["Search Nix code search for 'flake'"]
    );
    let _ = app.update(Message::Cancel);
    assert_eq!(app.search_query, "bm nixsearch");
    assert_eq!(command_list_titles(&app), ["Nix code search"]);

    // Typed before the bookmarks loaded: the keyword is known once they do.
    let app = bookmarks_app("bm nixsearch flake");
    assert_eq!(app.bookmarks.prompt.as_ref().unwrap().term, "flake");
}

#[test]
fn test_main_view_bookmark_keyword_opens_the_search_prompt() {
    let mut app = main_view_app(&["bookmarks"], Vec::new(), AddonsConfig::default());
    let library = super::bookmarks::fixture_library();
    let mut index = super::bookmarks::SearchIndex::new(&library);
    let _ = app.update(Message::SearchChanged("nixsearch".into()));
    assert!(app.bookmarks.prompt.is_none());
    *app.bookmark_searches.lock().unwrap() = std::mem::take(&mut index);

    let _ = app.update(Message::SearchChanged("nixsearch nixpkgs".into()));
    assert_eq!(app.bookmarks.prompt.as_ref().unwrap().term, "nixpkgs");
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv))
            if argv == ["firefox", "https://github.com/search?q=%22nixpkgs%22+path%3A*.nix"]
    ));

    // An addon trigger wins over a keyword of the same name.
    let mut app = main_view_app(&["bookmarks"], Vec::new(), AddonsConfig::default());
    let mut library = library;
    library.sources[0].root.folders[1].bookmarks[1].keyword = Some("bm".into());
    *app.bookmark_searches.lock().unwrap() = super::bookmarks::SearchIndex::new(&library);
    let _ = app.update(Message::SearchChanged("bm nix".into()));
    assert!(app.bookmarks.prompt.is_none());
}

#[test]
fn test_main_view_lists_bookmarks_and_opens_them() {
    let mut app = main_view_app(&["bookmarks"], Vec::new(), AddonsConfig::default());
    let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
        panic!("command list source");
    };
    let generation = list.generation;
    let options = app.bookmark_options();
    let _ = app.update(Message::MainViewLoaded(
        0,
        generation,
        Ok(super::bookmarks::flat_items(
            &super::bookmarks::fixture_library(),
            options,
        )),
    ));
    let _ = app.update(Message::SearchChanged("work".into()));
    assert_eq!(main_view_titles(&app), ["Raffi", "Rust docs"]);

    let _ = app.update(Message::SearchChanged("raffi".into()));
    assert_eq!(main_view_titles(&app), ["Raffi"]);
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv)) if argv == ["firefox", "https://github.com/chmouel/raffi"]
    ));
}

#[test]
fn test_reveal_offset_scrolls_only_when_the_row_is_hidden() {
    // A 400px viewport scrolled to 1000.
    assert_eq!(reveal_offset(1100.0, 60.0, 1000.0, 400.0), None);
    // Above the viewport: its top aligns with the top.
    assert_eq!(reveal_offset(900.0, 60.0, 1000.0, 400.0), Some(900.0));
    // Partly below: its bottom aligns with the bottom.
    assert_eq!(reveal_offset(1380.0, 60.0, 1000.0, 400.0), Some(1040.0));
    // Taller than the viewport: its top shows.
    assert_eq!(reveal_offset(1500.0, 500.0, 1000.0, 400.0), Some(1500.0));
}

#[test]
fn test_search_change_scrolls_back_only_when_the_list_is_scrolled() {
    let mut app = test_app(AddonsConfig::default());
    assert_eq!(app.update(Message::SearchChanged("a".into())).units(), 0);
    // A list scrolled down keeps its offset while the selection resets.
    let _ = app.update(Message::ListScrolled(0.5));
    assert!(app.update(Message::SearchChanged("ab".into())).units() > 0);
    let _ = app.update(Message::ListScrolled(0.0));
    assert_eq!(app.update(Message::SearchChanged("a".into())).units(), 0);
}

fn browser_history_app(query: &str) -> LauncherApp {
    let mut app = test_app(AddonsConfig::default());
    app.history.max_items = 0;
    let task = app.update(Message::SearchChanged(query.into()));
    assert!(task.units() > 0, "the history loads");
    assert_eq!(
        app.command_list.status(),
        Some("Loading browser history...")
    );
    let generation = app.command_list.generation;
    let init = super::browser_history::BrowserHistoryInit {
        groups: super::browser_history::fixture_groups(),
        errors: Vec::new(),
        library: Some(super::browser_history::fixture_group_library("Firefox")),
    };
    let _ = app.update(Message::BrowserHistoryInitialized(
        generation,
        Some(Arc::new(init)),
    ));
    app
}

#[test]
fn test_route_query_browser_history_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(
        route_query("hist raffi", &addons),
        QueryMode::CommandList {
            addon: CommandListAddon::BrowserHistory,
            query: "raffi".into()
        }
    );
    assert!(suggested_keywords("history", &addons).contains(&"hist".to_string()));

    addons.browser_history.enabled = false;
    assert_eq!(route_query("hist raffi", &addons), QueryMode::Standard);
}

#[test]
fn test_browser_history_ctrl_tab_cycles_browser_views() {
    let mut app = browser_history_app("hist ");
    assert_eq!(app.cycle_view_label().as_deref(), Some("Firefox"));
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);

    let task = app.update(Message::CycleView(false));
    assert!(task.units() > 0, "the next browser loads lazily");
    assert_eq!(app.cycle_view_label().as_deref(), Some("Brave"));
    assert_eq!(
        app.command_list.status(),
        Some("Loading browser history...")
    );

    let key = app.browser_history.active_group().unwrap().key.clone();
    let generation = app.command_list.generation;
    let _ = app.update(Message::BrowserHistoryGroupLoaded(
        generation,
        key,
        Some(Arc::new(super::browser_history::fixture_group_library(
            "Brave",
        ))),
    ));
    assert_eq!(command_list_titles(&app), ["Raffi on GitHub"]);

    let task = app.update(Message::CycleView(true));
    assert_eq!(task.units(), 0, "a cached browser shows at once");
    assert_eq!(app.cycle_view_label().as_deref(), Some("Firefox"));
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);

    let task = app.update(Message::CycleView(true));
    assert_eq!(task.units(), 0, "backwards wraps to the cached browser");
    assert_eq!(app.cycle_view_label().as_deref(), Some("Brave"));
    assert_eq!(command_list_titles(&app), ["Raffi on GitHub"]);
}

#[test]
fn test_browser_history_ignores_a_stale_browser_load() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::CycleView(false));
    let stale_generation = app.command_list.generation;
    let stale_key = app.browser_history.active_group().unwrap().key.clone();

    let _ = app.update(Message::CycleView(true));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Firefox"));
    let _ = app.update(Message::BrowserHistoryGroupLoaded(
        stale_generation,
        stale_key,
        Some(Arc::new(super::browser_history::fixture_group_library(
            "Brave",
        ))),
    ));
    assert_eq!(app.cycle_view_label().as_deref(), Some("Firefox"));
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);
}

#[test]
fn test_browser_history_search_waits_for_the_debounce() {
    let mut app = browser_history_app("hist ");
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);

    let _ = app.update(Message::SearchChanged("hist docs".into()));
    assert!(app.browser_history.search_pending);
    assert_eq!(
        command_list_titles(&app),
        ["Raffi", "Rust docs"],
        "the old rows stay while typing"
    );

    let session = app.browser_history.session;
    let generation = app.browser_history.search_generation;
    // Moving through the old rows must not leave the selection past the new ones.
    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::BrowserHistorySearchDue(session, generation));
    assert!(!app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Rust docs"]);
    assert_eq!(app.selected_index, app.command_list_offset());
}

#[test]
fn test_browser_history_ignores_a_stale_debounce() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist rust".into()));
    let stale_generation = app.browser_history.search_generation;

    let _ = app.update(Message::SearchChanged("hist github".into()));
    let generation = app.browser_history.search_generation;
    let session = app.browser_history.session;
    let _ = app.update(Message::BrowserHistorySearchDue(session, stale_generation));
    assert!(app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);

    let _ = app.update(Message::BrowserHistorySearchDue(session, generation));
    assert!(!app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Raffi"]);
}

#[test]
fn test_browser_history_empty_query_refreshes_immediately() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist rust".into()));
    assert!(app.browser_history.search_pending);

    let _ = app.update(Message::SearchChanged("hist ".into()));
    assert!(!app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);
}

#[test]
fn test_browser_history_enter_flushes_the_pending_search() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist docs".into()));
    assert!(app.browser_history.search_pending);

    let _ = app.update(Message::Submit);
    assert!(!app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Rust docs"]);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv)) if argv == ["firefox", "https://doc.rust-lang.org/std/"]
    ));
}

#[test]
fn test_browser_history_click_flushes_without_activating_a_stale_row() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist docs".into()));

    let _ = app.update(Message::CommandListSelected(0));
    assert!(!app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Rust docs"]);
    assert!(deferred_action(&app).is_none());
}

#[test]
fn test_browser_history_structured_filters_use_the_debounce() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist site:github.com".into()));
    assert!(app.browser_history.search_pending);
    assert_eq!(command_list_titles(&app), ["Raffi", "Rust docs"]);

    let session = app.browser_history.session;
    let generation = app.browser_history.search_generation;
    let _ = app.update(Message::BrowserHistorySearchDue(session, generation));
    assert_eq!(command_list_titles(&app), ["Raffi"]);
}

#[test]
fn test_browser_history_uses_literal_substring_search() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist gtb".into()));
    let session = app.browser_history.session;
    let generation = app.browser_history.search_generation;
    let _ = app.update(Message::BrowserHistorySearchDue(session, generation));
    assert!(
        command_list_titles(&app).is_empty(),
        "a fuzzy subsequence of github.com must not match"
    );
}

#[test]
fn test_browser_history_stale_debounce_after_leaving_is_ignored() {
    let mut app = browser_history_app("hist ");
    let _ = app.update(Message::SearchChanged("hist rust".into()));
    let session = app.browser_history.session;
    let generation = app.browser_history.search_generation;

    let _ = app.update(Message::SearchChanged("zzzz".into()));
    let before: Vec<String> = command_list_titles(&app)
        .into_iter()
        .map(str::to_string)
        .collect();
    let _ = app.update(Message::BrowserHistorySearchDue(session, generation));
    assert_eq!(command_list_titles(&app), before);
}

#[test]
fn test_browser_history_filters_and_opens_pages() {
    let mut app = browser_history_app("hist site:github.com");
    assert_eq!(command_list_titles(&app), ["Raffi"]);
    let _ = app.update(Message::CommandListSelected(0));
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv)) if argv == ["firefox", "https://github.com/chmouel/raffi"]
    ));
}

#[test]
fn test_main_view_lists_browser_history_and_opens_pages() {
    let mut app = main_view_app(&["browser_history"], Vec::new(), AddonsConfig::default());
    let super::main_view::Source::List(list) = &app.main_view.sources[0] else {
        panic!("command list source");
    };
    let generation = list.generation;
    let options = app.browser_history_options();
    let _ = app.update(Message::MainViewLoaded(
        0,
        generation,
        Ok(super::browser_history::flat_items(
            &super::browser_history::fixture_library(),
            options,
        )),
    ));
    let _ = app.update(Message::SearchChanged("raffi".into()));
    assert_eq!(main_view_titles(&app), ["Raffi on GitHub"]);
    let _ = app.update(Message::Submit);
    assert!(matches!(
        deferred_action(&app),
        Some(DeferredAction::Command(argv)) if argv == ["firefox", "https://github.com/chmouel/raffi?utm_source=news"]
    ));
}
