use super::actions::DeferredAction;
use super::app::{
    classify_query, fido_restart, fido_updir, is_single_insert, keyword_suggestions,
    loggable_query, route_query, tilde_path, QueryMode,
};
use super::command_list::CommandListAddon;
use super::help::{filter_help_entries, help_entries, HelpSection};
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
        dynamic_preview: None,
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
fn test_is_single_insert() {
    assert!(is_single_insert("abc", "abc.", '.'));
    assert!(is_single_insert("abc", "a.bc", '.'));
    assert!(is_single_insert("", ".", '.'));
    assert!(is_single_insert("a.", "a..", '.'));
    assert!(is_single_insert("é", ".é", '.'));
    assert!(!is_single_insert("abc", "abc", '.'));
    assert!(!is_single_insert("abc", "abcx", '.'));
    assert!(!is_single_insert("abc", "ab.", '.'));
    assert!(!is_single_insert("abc", "abc..", '.'));
}

#[test]
fn test_file_browser_ctrl_period_does_not_type_a_period() {
    let dir = std::env::temp_dir().join(format!("raffi-fb-ctrl-period-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let root = format!("{}/", dir.display());

    let mut app = test_app(AddonsConfig::default());
    let _ = app.update(Message::SearchChanged(root.clone()));
    assert!(app.file_browser.active);

    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::CTRL));
    let _ = app.update(Message::SearchChanged(format!("{root}.")));
    let _ = app.update(Message::FileBrowserCycleShow);
    assert_eq!(app.search_query, root);
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Files);

    let _ = app.update(Message::ModifiersChanged(iced::keyboard::Modifiers::empty()));
    let _ = app.update(Message::SearchChanged(format!("{root}.")));
    assert_eq!(app.search_query, format!("{root}."));

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
    let _ = app.update(Message::FileBrowserCycleShow);
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::All);

    let _ = app.update(Message::SearchChanged(root.clone()));
    assert_eq!(names(&app), vec!["nest", "sub", "a.txt"]);

    let _ = app.update(Message::FileBrowserCycleShow);
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
        Some("No files here (Ctrl+. for directories)")
    );

    let _ = app.update(Message::FileBrowserCycleShow);
    let _ = app.update(Message::SearchChanged(format!("{root}sub/")));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Directories);
    assert!(names(&app).is_empty());
    assert_eq!(
        app.file_browser.error.as_deref(),
        Some("No directories here (Ctrl+. to show all)")
    );

    // The mode survives leaving and re-entering the browser.
    let _ = app.update(Message::SearchChanged(String::new()));
    assert!(!app.file_browser.active);
    let _ = app.update(Message::SearchChanged(root.clone()));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::Directories);
    assert_eq!(names(&app), vec!["nest", "sub"]);

    let _ = app.update(Message::FileBrowserCycleShow);
    let _ = app.update(Message::SearchChanged(root));
    assert_eq!(app.file_browser.show, crate::FileBrowserShow::All);
    assert_eq!(names(&app), vec!["nest", "sub", "a.txt"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_ctrl_period_maps_on_any_layout() {
    use iced::keyboard::{Key, Modifiers};
    let period = Key::Character(".".into());
    let semicolon = Key::Character(";".into());

    assert!(super::view::is_ctrl_period(
        &period,
        &period,
        Modifiers::CTRL
    ));
    // Layouts where "." needs Shift report the unshifted key in `key`.
    assert!(super::view::is_ctrl_period(
        &semicolon,
        &period,
        Modifiers::CTRL | Modifiers::SHIFT
    ));
    assert!(!super::view::is_ctrl_period(
        &period,
        &period,
        Modifiers::empty()
    ));
    assert!(!super::view::is_ctrl_period(
        &semicolon,
        &semicolon,
        Modifiers::CTRL
    ));
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
        dynamic_preview: None,
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
        dynamic_preview: None,
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
            "win",
            "tabs",
            "media",
            "audio",
            "kill",
            "bt",
            "pass",
            "bw",
            "dev",
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
            "Ctrl+H",
            "Ctrl+O",
            "Ctrl+B",
            "Ctrl+Y",
            "Ctrl+Alt+Y",
            "Ctrl+S",
            "Ctrl+.",
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
    assert_eq!(entries[15].completion, None);
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
    addons.windows.enabled = false;
    addons.browser_tabs.enabled = false;
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
fn test_preview_ansi_palette_swaps_unreadable_colours() {
    use super::theme::PreviewStyle;
    let config = crate::PreviewConfig::default();
    // Palette black sits on a dark background: unreadable, so muted.
    let dark = ThemeColors::from_mode_with_overrides(&crate::ThemeMode::Dark, None);
    let style = PreviewStyle::resolve(&dark, &config, None);
    assert_eq!(style.ansi[0], style.text_muted);
    assert_eq!(style.ansi[1], dark.ansi[1]);
    // Light palettes put a pale colour in slot 0 and a dark one in slot 7.
    let latte = ThemeColors::from_mode_with_overrides(&crate::ThemeMode::CatppuccinLatte, None);
    let style = PreviewStyle::resolve(&latte, &config, None);
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
                subtitle: Some("Fallback subtitle".into()),
                arg: Some("arg2".into()),
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
        subtitle: None,
        arg: None,
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
        subtitle: None,
        arg: Some(format!("{title}-arg")),
        icon: None,
        preview: Some(super::types::ScriptFilterPreview::Text(
            "static body".into(),
        )),
        quicklookurl: None,
        variables: Default::default(),
        text: None,
    }
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
    let keep_open = !app.addons.media.close_on_action;
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
    addons.media.close_on_action = true;
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
    addons.audio.close_on_action = true;
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
    addons.bluetooth.close_on_action = true;
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
    assert_eq!(app.total_items(), 0, "addon rows wait for a query");

    let _ = app.update(Message::SearchChanged("mail".into()));
    assert!(app.filtered_configs.is_empty());
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
        2,
        "unsupported entries are skipped"
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
    });
    let mut app = main_view_app(&["script_filters.Issues"], Vec::new(), addons);
    let result = |title: &str| ScriptFilterResult {
        default_icon: None,
        items: vec![ScriptFilterItem {
            title: title.into(),
            subtitle: None,
            arg: None,
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
            title: format!("issue {index}"),
            subtitle: None,
            arg: None,
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
                subtitle: None,
                arg: None,
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
}

#[test]
fn test_main_view_ctrl_tab_focuses_each_source_then_all() {
    let mut addons = AddonsConfig::default();
    addons.web_searches.push(WebSearchConfig {
        name: "Google".into(),
        keyword: "g".into(),
        url: "https://google.com/search?q={query}".into(),
        icon: None,
    });
    let mut app = test_app_with_fallbacks(addons, vec!["addons.web_searches.Google".into()]);
    app.configs = firefox_launcher();
    let entries = ["browser_tabs", "launchers", "system"].map(String::from);
    let _ = app.configure_main_view(Some(&entries));
    load_main_view_tabs(&mut app);
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.total_items(), 1, "the launcher, tabs wait for a query");
    assert_eq!(app.main_view_focus_label(), "All");

    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.main_view_focus_label(), "Tabs");
    assert!(app.filtered_configs.is_empty(), "launchers are hidden");
    assert_eq!(app.total_items(), 2, "every tab, without a query");

    let _ = app.update(Message::SearchChanged("mail".into()));
    assert_eq!(app.main_view_focus_label(), "Tabs", "the focus stays");
    assert_eq!(app.fallback_count(), 0, "fallbacks only show in All");
    assert_eq!(app.total_items(), 1);
    assert_eq!(app.resolve_selected_preview().unwrap().title, "Mail");

    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.main_view_focus_label(), "Launchers");
    assert_eq!(app.filtered_configs, vec![0]);
    assert_eq!(app.total_items(), 1, "no tab row");

    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.main_view_focus_label(), "System");
    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.main_view_focus_label(), "All");
    assert_eq!(app.total_items(), 1 + 1 + 1, "tab, launcher and fallback");

    let _ = app.update(Message::MoveDown);
    let _ = app.update(Message::MainViewCycle(true));
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

    let _ = app.update(Message::MainViewCycle(false));
    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.main_view_focus_label(), "Notes");
    assert_eq!(app.total_items(), 8, "no cap on a focused source");
    let _ = app.update(Message::SearchChanged(String::new()));
    assert_eq!(app.total_items(), 8);

    let _ = app.update(Message::SearchChanged("1+1".into()));
    assert!(app.inline_result.is_none(), "inline rows only show in All");
    let _ = app.update(Message::MainViewCycle(false));
    assert!(app.inline_result.is_some());
}

#[test]
fn test_ctrl_tab_completes_where_the_main_view_does_not_show() {
    let mut app = test_app(snippet_addons());
    let _ = app.update(Message::SearchChanged("sn".into()));
    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.search_query, "sn ", "no main_view configured");

    let mut app = main_view_app(&["text_snippets.Snippets"], Vec::new(), snippet_addons());
    let _ = app.update(Message::SearchChanged("sn em".into()));
    let _ = app.update(Message::MainViewCycle(false));
    assert_eq!(app.main_view.focus, None, "keyword modes keep their list");
}
