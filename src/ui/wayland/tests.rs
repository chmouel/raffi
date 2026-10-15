use super::app::{keyword_suggestions, route_query, QueryMode};
use super::help::{filter_help_entries, help_entries};
use super::placeholders::Expanded;
use super::preview::{
    format_byte_size, inspect_path, PreviewBody, MAX_FILE_PREVIEW_BYTES, MAX_FILE_PREVIEW_LINES,
};
use super::state::{
    ClipboardData, CurrencyConversionRequest, CurrencyResult, EmojiEntry, FallbackAction,
    KeywordSuggestion, LauncherApp, Message, ScriptFilterResult,
};
use super::theme::ThemeColors;
use super::types::{ScriptFilterItem, ScriptFilterItemText};
use crate::ui::FontSizes;
use crate::{
    AddonsConfig, RaffiConfig, ScriptFilterConfig, SortMode, TextSnippet, TextSnippetSourceConfig,
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
        assert!(app.deferred_paste.lock().unwrap().is_none());
        assert!(app.text_snippets.pending.is_some());
        let _ = app.update(Message::HyprPastePrepared(
            session,
            Err("wl-copy failed".into()),
        ));
        assert_eq!(app.text_snippets.error.as_deref(), Some("wl-copy failed"));
        assert!(app.text_snippets.active);
        assert!(app.text_snippets.pending.is_none());
        assert!(app.deferred_paste.lock().unwrap().is_none());

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
        assert!(app.deferred_paste.lock().unwrap().is_some());
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
    assert!(app.deferred_paste.lock().unwrap().is_none());
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
            "math",
            "$",
            "↑/↓",
            "Ctrl+J",
            "Ctrl+K",
            "Ctrl+P",
            "Ctrl+H",
            "Alt+P/N",
            "Tab",
            "Ctrl+/",
            "Esc",
            "Enter",
            "Ctrl+Enter"
        ]
    );
    assert_eq!(entries[1].icon.as_deref(), Some("gist-icon"));
    assert_eq!(entries[1].completion.as_deref(), Some("gist "));
    assert_eq!(entries[0].completion.as_deref(), Some("~/"));
    assert_eq!(entries[5].completion, None);
    // Shortcut rows are informational: they never arm a keyword.
    for entry in entries.iter().skip(7) {
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
    let entries = help_entries(&addons);

    // With every addon disabled only the shortcut rows remain.
    let triggers: Vec<&str> = entries.iter().map(|entry| entry.trigger.as_str()).collect();
    assert_eq!(
        triggers,
        vec![
            "↑/↓",
            "Ctrl+J",
            "Ctrl+K",
            "Ctrl+P",
            "Ctrl+H",
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
            "Ctrl+H",
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

    // Disabled addons drop their keyword rows; the keyboard shortcut rows stay.
    let entries = help_entries(&addons);
    assert!(entries.iter().all(|entry| entry.completion.is_none()));
    assert_eq!(entries.len(), 11);
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

#[test]
fn test_preview_scroll_messages_leave_state_unchanged() {
    let mut app = test_app(AddonsConfig::default());
    super::configure_preview_visibility(&mut app.view, Some(false));
    app.calculator_result = Some(super::types::CalculatorResult {
        expression: "1+1".into(),
        result: 2.0,
    });
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
    app.calculator_result = Some(super::types::CalculatorResult {
        expression: "1+1".into(),
        result: 2.0,
    });
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
    let file_url = format!("file://{}", file_path.to_string_lossy().replace(' ', "%20"));

    let mut app = test_app(AddonsConfig::default());
    app.script_filter.results = Some(ScriptFilterResult {
        default_icon: None,
        items: vec![ScriptFilterItem {
            title: "File".into(),
            subtitle: None,
            arg: None,
            icon: None,
            preview: None,
            quicklookurl: Some(file_url),
            text: None,
        }],
    });

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
