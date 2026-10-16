use super::actions::DeferredAction;
use super::app::{
    classify_query, fido_updir, keyword_suggestions, loggable_query, route_query, QueryMode,
};
use super::help::{filter_help_entries, help_entries, HelpSection};
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
use super::types::{InlineProvider, ScriptFilterItem, ScriptFilterItemText};
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
    app.program_exists = |_| true;
    app
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
            subtitle: None,
            arg: None,
            icon: None,
            preview: None,
            quicklookurl: Some("file:///proc/cpuinfo".into()),
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
    });
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
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
    app.file_browser.hovered_row = Some(0);
    app.file_browser.pressed_row = Some(0);
    app.file_browser.dragging = true;

    app.file_browser.clear();

    assert!(!app.file_browser.active);
    assert!(app.file_browser.current_dir.is_empty());
    assert!(app.file_browser.entries.is_empty());
    assert!(app.file_browser.error.is_none());
    assert!(app.file_browser.hovered_row.is_none());
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
fn test_file_browser_hover_tracks_the_row_under_the_pointer() {
    let mut app = browser_app_with_two_dirs();

    let _ = app.update(Message::FileBrowserHover(1));
    assert_eq!(app.file_browser.hovered_row, Some(1));

    // Leaving a different row must not clear the current hover.
    let _ = app.update(Message::FileBrowserUnhover(0));
    assert_eq!(app.file_browser.hovered_row, Some(1));

    let _ = app.update(Message::FileBrowserUnhover(1));
    assert_eq!(app.file_browser.hovered_row, None);
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
fn test_drag_events_drive_browser_dragging_state() {
    let mut app = browser_app_with_two_dirs();
    app.file_browser.pressed_row = Some(0);
    app.file_browser.hovered_row = Some(0);

    let _ = app.update(Message::DragEvent(super::dnd::DragEvent::Started));
    assert!(app.file_browser.dragging);
    // The compositor consumed the pointer: press and hover tracking reset.
    assert!(app.file_browser.pressed_row.is_none());
    assert!(app.file_browser.hovered_row.is_none());

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
fn test_file_browser_query_change_clears_stale_hover() {
    let mut app = browser_app_with_two_dirs();
    app.file_browser.hovered_row = Some(1);

    let _ = app.update(Message::SearchChanged("/tmp/al".into()));

    assert!(app.file_browser.active);
    assert_eq!(app.file_browser.hovered_row, None);
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
        vec![
            "~/",
            "gist",
            "sn",
            "emoji",
            "gh",
            "sys",
            "math",
            "$",
            "units",
            "↑/↓",
            "Ctrl+J",
            "Ctrl+K",
            "Ctrl+P",
            "Ctrl+H",
            "Ctrl+O",
            "Ctrl+S",
            "Alt+P/N",
            "Tab",
            "Backspace",
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
    assert_eq!(entries[6].completion, None);
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
    addons.units.enabled = false;
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
        vec!["Ctrl+J", "Ctrl+K", "Ctrl+P", "Ctrl+/", "Ctrl+Enter"]
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
    addons.units.enabled = false;

    // Disabled addons drop their keyword rows and addon-specific shortcuts.
    let entries = help_entries(&addons);
    assert!(entries.iter().all(|entry| entry.completion.is_none()));
    assert!(entries
        .iter()
        .all(|entry| entry.section == HelpSection::KeyboardShortcuts));
    assert_eq!(entries.len(), 10);
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

    // Resizing the window grows the full-pane image preview with it.
    let _ = app.update(Message::WindowResized(iced::Size::new(1200.0, 900.0)));
    assert_eq!(app.view.window_size.width, 1200.0);
    assert_eq!(app.preview_viewport_height(), 722.0);

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

    let default_style = PreviewStyle::resolve(&theme, &crate::PreviewConfig::default(), None);
    assert_eq!(default_style.text, PreviewStyle::DEFAULT_FONT_SIZE);
    assert_eq!(default_style.width_fraction, 0.5);
    assert_eq!(default_style.accent, theme.accent);

    let inherited_font_style =
        PreviewStyle::resolve(&theme, &crate::PreviewConfig::default(), Some("UI Font"));
    assert!(matches!(
        inherited_font_style.font.family,
        iced::font::Family::Name("UI Font")
    ));

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
    let style = PreviewStyle::resolve(&theme, &config, Some("UI Font"));
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
                subtitle: Some("Fallback subtitle".into()),
                arg: Some("arg1".into()),
                icon: None,
                preview: Some("Explicit preview body".into()),
                quicklookurl: None,
                text: Some(ScriptFilterItemText {
                    copy: Some("copy1".into()),
                    largetype: Some("Large type body".into()),
                }),
            },
            ScriptFilterItem {
                title: "Item 2".into(),
                subtitle: Some("Fallback subtitle".into()),
                arg: Some("arg2".into()),
                icon: None,
                preview: None,
                quicklookurl: None,
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
        subtitle: None,
        arg: None,
        icon: None,
        preview: None,
        quicklookurl: Some(quicklookurl),
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

#[test]
fn test_preview_emoji_calculator_currency_and_web_search() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "GitHub".into(),
        keyword: "gh".into(),
        url: "https://github.com/search?q={query}".into(),
        icon: None,
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
        PreviewBody::Text { content, .. } if content == "(Empty file)"
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
fn test_backspace_updir_disabled_is_a_plain_delete() {
    let dir = updir_test_dir();
    let base = dir.to_string_lossy().into_owned();
    let mut addons = AddonsConfig::default();
    addons.file_browser.backspace_updir = false;
    let mut app = test_app(addons);

    let _ = app.update(Message::SearchChanged(format!("{base}/gamma/")));
    let _ = app.update(Message::SearchChanged(format!("{base}/gamma")));

    assert_eq!(app.search_query, format!("{base}/gamma"));
    assert_eq!(app.selected_index, 0);

    std::fs::remove_dir_all(&dir).unwrap();
}

fn clipboard_addons() -> AddonsConfig {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = true;
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
            images_only: false,
        }
    );
    assert_eq!(route_query("clip", &addons), QueryMode::Standard);
    assert_eq!(
        route_query("clipimg cat", &addons),
        QueryMode::Clipboard {
            query: "cat".into(),
            images_only: true,
        }
    );
    let suggestions = keyword_suggestions("cli", &addons);
    let keywords: Vec<_> = suggestions.iter().map(|s| s.keyword.as_str()).collect();
    assert_eq!(keywords, vec!["clip", "clipimg"]);
    assert_eq!(suggestions[0].icon.as_deref(), Some("copyq"));

    addons.clipboard.trigger = Some("cb".into());
    assert_eq!(
        route_query("cb x", &addons),
        QueryMode::Clipboard {
            query: "x".into(),
            images_only: false,
        }
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
        Some(PreviewBody::Text { content, .. }) if content == "Loading preview..."
    ));
    assert!(preview
        .metadata
        .iter()
        .any(|(key, value)| key == "Ctrl+I" && value == "Show only images"));
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
        Some(PreviewBody::Text { content, .. }) if content == "beta\nsecond line"
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
fn test_clipboard_images_only_keyword_and_toggle() {
    let mut app = test_app(clipboard_addons());
    open_clipboard(&mut app, "clip ");
    assert!(!app.clipboard.images_only);

    let generation = app.clipboard.generation;
    let _ = app.update(Message::ClipboardToggleImages);
    assert!(app.clipboard.images_only);
    assert_ne!(
        app.clipboard.generation, generation,
        "rescans with the filter"
    );
    assert_eq!(app.clipboard.status(), "Searching clipboard images...");

    // Typing keeps the Ctrl+I choice.
    open_clipboard(&mut app, "clip cat");
    assert!(app.clipboard.images_only);
    let _ = app.update(Message::ClipboardPage(
        app.clipboard.generation,
        Ok(clip_page("h", 1, 1, Vec::new())),
    ));
    assert_eq!(app.clipboard.status(), "No clipboard images match");

    // The images keyword starts filtered, and Ctrl+I shows everything again.
    open_clipboard(&mut app, "clipimg ");
    assert!(app.clipboard.images_only);
    let _ = app.update(Message::ClipboardToggleImages);
    assert!(!app.clipboard.images_only);
    open_clipboard(&mut app, "clip ");
    assert!(!app.clipboard.images_only);

    let _ = app.update(Message::SearchChanged("fire".into()));
    let _ = app.update(Message::ClipboardToggleImages);
    assert!(!app.clipboard.active);
    assert!(!app.clipboard.images_only);
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

fn open_system(app: &mut LauncherApp, query: &str) {
    app.history.max_items = 0;
    let _ = app.update(Message::SearchChanged(query.into()));
    assert!(app.system.active);
}

fn deferred_argv(app: &LauncherApp) -> Option<Vec<String>> {
    match app.deferred_action.lock().unwrap().as_ref()? {
        DeferredAction::Command(argv) => Some(argv.clone()),
        other => panic!("unexpected deferred action {other:?}"),
    }
}

fn system_titles(app: &LauncherApp) -> Vec<&'static str> {
    app.system
        .filtered
        .iter()
        .map(|&index| app.system.entries[index].title)
        .collect()
}

#[test]
fn test_route_query_system_trigger() {
    let mut addons = AddonsConfig::default();
    assert_eq!(route_query("sys", &addons), QueryMode::Standard);
    assert_eq!(
        route_query("sys ", &addons),
        QueryMode::System {
            query: String::new()
        }
    );
    assert_eq!(
        route_query(" sys reb", &addons),
        QueryMode::System {
            query: "reb".into()
        }
    );

    addons.system.trigger = Some("power".into());
    assert_eq!(route_query("sys reb", &addons), QueryMode::Standard);
    assert!(matches!(
        route_query("power reb", &addons),
        QueryMode::System { .. }
    ));

    addons.system.enabled = false;
    assert_eq!(route_query("power reb", &addons), QueryMode::Standard);
    assert!(keyword_suggestions("power", &addons).is_empty());
}

#[test]
fn test_default_triggers_do_not_shadow_each_other_or_web_searches() {
    let mut addons = AddonsConfig::default();
    addons.clipboard.enabled = true;
    assert!(matches!(
        route_query("emoji x", &addons),
        QueryMode::Emoji { .. }
    ));
    assert!(matches!(
        route_query("clip x", &addons),
        QueryMode::Clipboard {
            images_only: false,
            ..
        }
    ));
    assert!(matches!(
        route_query("clipimg x", &addons),
        QueryMode::Clipboard {
            images_only: true,
            ..
        }
    ));
    assert!(matches!(
        route_query("sys x", &addons),
        QueryMode::System { .. }
    ));

    // A user web search keeps its keyword, and its suggestion comes first.
    addons.web_searches.push(WebSearchConfig {
        name: "Sysdig".into(),
        keyword: "sys".into(),
        url: "https://example.invalid?q={query}".into(),
        icon: None,
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
            "Lock, suspend, log out, reboot or shut down"
        ]
    );
}

#[test]
fn test_system_list_filters_entries() {
    let mut app = test_app(AddonsConfig::default());
    open_system(&mut app, "sys ");
    assert_eq!(
        system_titles(&app),
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
    assert_eq!(system_titles(&app), vec!["Reboot"]);
    open_system(&mut app, "sys zzz");
    assert_eq!(app.total_items(), 0);

    let _ = app.update(Message::SearchChanged("firefox".into()));
    assert!(!app.system.active);
    assert!(app.system.entries.is_empty());
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
    assert!(app.system.armed.is_some());
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
    assert!(app.system.armed.is_none());
    let _ = app.update(Message::Submit);
    assert!(app.system.armed.is_some());

    let _ = app.update(Message::SearchChanged("sys re".into()));
    assert!(app.system.armed.is_none());
    let _ = app.update(Message::Submit);

    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    assert!(app.system.armed.is_none());
    let _ = app.update(Message::Submit);
    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::empty()));
    let _ = app.update(Message::Submit);
    assert!(app.system.armed.is_some());

    // Clicking another row arms that row instead of running the first.
    let _ = app.update(Message::SearchChanged("sys ".into()));
    let _ = app.update(Message::ItemClicked(4));
    let _ = app.update(Message::ItemClicked(5));
    assert_eq!(
        app.system.armed.map(|(id, _)| id),
        Some(crate::SystemCommand::Shutdown)
    );

    let _ = app.update(Message::ToggleHelp);
    assert!(app.system.armed.is_none());
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

    let entries = system_entries(&addons.system, true, None, |_| true);
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
    let logout = |hyprland, session| {
        system_entries(&config, hyprland, session, |_| true)
            .into_iter()
            .find(|entry| entry.id == crate::SystemCommand::Logout)
            .unwrap()
            .action
    };
    assert!(matches!(
        logout(true, Some("3")),
        DeferredAction::HyprDispatch(args) if args == ["exit"]
    ));
    assert!(matches!(
        logout(false, Some("3")),
        DeferredAction::Command(argv) if argv == ["loginctl", "terminate-session", "3"]
    ));
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
    let entries = system_entries(&addons.system, false, Some("2"), |program| {
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
        app.system
            .filtered
            .iter()
            .position(|&index| app.system.entries[index].id == id)
            .unwrap()
    };
    let reboot = position(&app, crate::SystemCommand::Reboot);
    let lock = position(&app, crate::SystemCommand::Lock);

    let _ = app.update(Message::ItemClicked(reboot));
    assert!(app.system.armed.is_some());
    let _ = app.update(Message::ItemClicked(lock));
    assert!(app.system.armed.is_none());
    let _ = app.update(Message::ItemClicked(reboot));
    assert_eq!(deferred_argv(&app), None);
    assert!(app.system.armed.is_some());
}

#[test]
fn test_system_logout_without_session_is_not_actionable() {
    let config = crate::SystemAddonConfig::default();
    let logout = system_entries(&config, false, None, |_| true)
        .into_iter()
        .find(|entry| entry.id == crate::SystemCommand::Logout)
        .unwrap();
    assert_eq!(logout.missing.as_deref(), Some("XDG_SESSION_ID"));

    let mut config = crate::SystemAddonConfig::default();
    config.commands.logout = Some(vec!["uwsm".into(), "stop".into()]);
    let logout = system_entries(&config, false, None, |_| true)
        .into_iter()
        .find(|entry| entry.id == crate::SystemCommand::Logout)
        .unwrap();
    assert!(logout.missing.is_none());
}
