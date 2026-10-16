use std::collections::HashMap;
use std::fs;
use std::path::Path;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, move_cursor_to_end, scroll_by, snap_to};
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::Task;

use super::actions::{
    execute_action, execute_file_action, execute_snippet_action, open_in_file_manager, spawn_copy,
};
use super::browser::read_directory;
use super::currency::{
    fetch_exchange_rate, fetch_multi_exchange_rates, is_currency_help_query,
    try_parse_currency_conversion, try_parse_multi_currency_conversion,
};
use super::emoji::{
    download_and_load_emoji_data, emoji_fallback_entries, filter_emoji_into,
    load_emoji_data_from_disk, resolve_emoji_file_names,
};
use super::help::{filter_help_entries, help_entries};
use super::script_filters::{execute_script_filter, parse_script_filter_output};
use super::snippets::{
    compute_previews, execute_text_snippet_command, expand_final, fetch_clipboard, filter_snippets,
    missing_clipboard, parse_snippet_file, snippet_mru_key,
};
use super::state::{
    CachedRate, ClipboardData, CurrencyConversion, CurrencyConversionRequest, CurrencyResult,
    CurrencyState, EmojiEntry, EmojiState, FallbackAction, FileBrowserState, HistoryState,
    KeywordSuggestion, LauncherApp, Message, MultiCurrencyRequest, MultiCurrencyResult,
    PendingSelection, ResolvedFallback, ScriptFilterResult, ScriptFilterState, SharedSelection,
    TextSnippetState, ViewState, WebSearchActiveState, WebSearchState,
};
use super::support::{
    fuzzy_match_configs, load_history, load_mru_map, mru_bounds, mru_sort_key, record_mru_use,
    save_history, save_mru_map, try_evaluate_math,
};
use crate::ui::{get_mru_cache_path, get_snippet_mru_cache_path, FontSizes};
use crate::{
    read_desktop_icon_map, read_icon_map, resolve_launcher_icon_path, AddonsConfig, RaffiConfig,
    SortMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum QueryMode {
    FileBrowser { expanded: String, filter: String },
    ScriptFilter { config_index: usize, query: String },
    TextSnippet { config_index: usize, query: String },
    Emoji { query: String },
    WebSearch { config_index: usize, query: String },
    Standard,
}

/// Pixels scrolled per Ctrl+J / Ctrl+K press in the preview pane.
const PREVIEW_SCROLL_STEP: f32 = 120.0;

/// Match `keyword` followed by a space and return the remaining query.
///
/// `input` must only have had its leading whitespace stripped: the space right
/// after the keyword is what arms the addon, so a fully trimmed string cannot
/// be used here. `"sn"` yields `None`, `"sn "` yields an empty query.
fn extract_keyword_query(input: &str, keyword: &str) -> Option<String> {
    input
        .strip_prefix(keyword)
        .and_then(|rest| rest.strip_prefix(' '))
        .map(|rest| rest.trim().to_string())
}

/// Strip leading whitespace only, preserving a trailing space so keyword
/// triggers can tell `sn` apart from `sn `.
fn keyword_input(query: &str) -> &str {
    query.trim_start()
}

fn expand_file_browser_query(trimmed: &str) -> String {
    if trimmed == "~" {
        format!("{}/", std::env::var("HOME").unwrap_or_default())
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else {
        trimmed.to_string()
    }
}

/// Fido/ido-style "go up a directory": when `new` is `old` with its trailing
/// `/` deleted, return the parent directory query (with a trailing `/`) and
/// the name of the directory that was left. `~/` goes up to the parent of
/// `home`; deleting the root `/` is left alone.
pub(super) fn fido_updir(old: &str, new: &str, home: &str) -> Option<(String, String)> {
    if old.strip_suffix('/') != Some(new) {
        return None;
    }
    let dir = new.trim_start();
    let (parent, left) = if dir == "~" {
        let home = home.trim_end_matches('/');
        let slash = home.rfind('/')?;
        (home[..=slash].to_string(), &home[slash + 1..])
    } else {
        let slash = dir.rfind('/')?;
        (dir[..=slash].to_string(), &dir[slash + 1..])
    };
    if left.is_empty() {
        return None;
    }
    Some((parent, left.to_string()))
}

pub(super) fn route_query(query: &str, addons: &AddonsConfig) -> QueryMode {
    let input = keyword_input(query);
    let trimmed = input.trim_end();

    let is_file_browser_query = addons.file_browser.enabled
        && (trimmed.starts_with('/') || trimmed == "~" || trimmed.starts_with("~/"));
    if is_file_browser_query {
        let expanded = expand_file_browser_query(trimmed);
        let filter = if expanded.ends_with('/') {
            String::new()
        } else if let Some(last_slash) = expanded.rfind('/') {
            expanded[last_slash + 1..].to_string()
        } else {
            String::new()
        };
        return QueryMode::FileBrowser { expanded, filter };
    }

    if let Some((config_index, query)) =
        addons
            .script_filters
            .iter()
            .enumerate()
            .find_map(|(index, config)| {
                extract_keyword_query(input, &config.keyword).map(|query| (index, query))
            })
    {
        return QueryMode::ScriptFilter {
            config_index,
            query,
        };
    }

    if let Some((config_index, query)) =
        addons
            .text_snippets
            .iter()
            .enumerate()
            .find_map(|(index, config)| {
                extract_keyword_query(input, &config.keyword).map(|query| (index, query))
            })
    {
        return QueryMode::TextSnippet {
            config_index,
            query,
        };
    }

    let emoji_trigger = addons.emoji.trigger.as_deref().unwrap_or("emoji");
    if addons.emoji.enabled {
        if let Some(query) = extract_keyword_query(input, emoji_trigger) {
            return QueryMode::Emoji { query };
        }
    }

    if let Some((config_index, query)) =
        addons
            .web_searches
            .iter()
            .enumerate()
            .find_map(|(index, config)| {
                extract_keyword_query(input, &config.keyword).map(|query| (index, query))
            })
    {
        return QueryMode::WebSearch {
            config_index,
            query,
        };
    }

    QueryMode::Standard
}

/// Collect the keyword rows to offer for a partially typed keyword.
///
/// Matching is a case-insensitive prefix match, so typing `s` already lists
/// every keyword starting with `s`, and an exactly typed keyword still shows
/// its row. Returns nothing once the query contains whitespace, since that
/// means the keyword is already armed or the user is doing a normal search.
pub(super) fn keyword_suggestions(query: &str, addons: &AddonsConfig) -> Vec<KeywordSuggestion> {
    let input = query.trim_start();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    if addons.file_browser.enabled && (input.starts_with('/') || input.starts_with('~')) {
        return Vec::new();
    }

    let input_lower = input.to_lowercase();
    let matches = |keyword: &str| keyword.to_lowercase().starts_with(&input_lower);

    let mut suggestions = Vec::new();

    for config in &addons.script_filters {
        if matches(&config.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: config.keyword.clone(),
                subtitle: format!("Search {}", config.name),
                icon: config.icon.clone(),
            });
        }
    }

    for config in &addons.text_snippets {
        if matches(&config.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: config.keyword.clone(),
                subtitle: format!("Browse {}", config.name),
                icon: config.icon.clone(),
            });
        }
    }

    if addons.emoji.enabled {
        let trigger = addons.emoji.trigger.as_deref().unwrap_or("emoji");
        if matches(trigger) {
            suggestions.push(KeywordSuggestion {
                keyword: trigger.to_string(),
                subtitle: "Browse emoji and icons".to_string(),
                icon: None,
            });
        }
    }

    for config in &addons.web_searches {
        if matches(&config.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: config.keyword.clone(),
                subtitle: format!("Search {}", config.name),
                icon: config.icon.clone(),
            });
        }
    }

    suggestions
}

/// Fetch the raw Wayland handles of the main window, if any, so the
/// drag-and-drop worker can attach to the same connection. Runs once at
/// startup; on other windowing systems it resolves to `None`.
fn fetch_wayland_handles() -> Task<Message> {
    iced::window::oldest().then(|maybe_id| match maybe_id {
        Some(id) => {
            iced::window::run(id, super::dnd::extract_wayland_handles).map(Message::WaylandHandles)
        }
        None => Task::none(),
    })
}

impl LauncherApp {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        mut configs: Vec<RaffiConfig>,
        addons: AddonsConfig,
        no_icons: bool,
        selected_item: SharedSelection,
        initial_query: Option<String>,
        theme: super::theme::ThemeColors,
        max_history: u32,
        font_sizes: FontSizes,
        sort_mode: SortMode,
        fallback_paths: Vec<String>,
    ) -> (Self, Task<Message>) {
        crate::debug_log!(
            "LauncherApp::new: configs={} no_icons={} sort_mode={sort_mode:?} initial_query={initial_query:?}",
            configs.len(),
            no_icons
        );
        let icon_map = if no_icons {
            HashMap::new()
        } else {
            read_icon_map().unwrap_or_default()
        };
        let desktop_icon_map = if no_icons {
            HashMap::new()
        } else {
            read_desktop_icon_map()
        };

        let mru_map = load_mru_map(get_mru_cache_path().ok().as_deref());

        // Pre-compute aggregate stats for hybrid sorting
        let (max_count, min_ts, max_ts) = mru_bounds(&mru_map);

        configs.sort_by_key(|config| {
            let description = config
                .description
                .as_deref()
                .unwrap_or_else(|| config.binary.as_deref().unwrap_or(""));
            std::cmp::Reverse(mru_sort_key(
                description,
                &mru_map,
                &sort_mode,
                max_count,
                min_ts,
                max_ts,
            ))
        });

        let launcher_icon_paths = if no_icons {
            vec![None; configs.len()]
        } else {
            configs
                .iter()
                .map(|config| resolve_launcher_icon_path(config, &icon_map, &desktop_icon_map))
                .collect()
        };

        let initial_query = initial_query.unwrap_or_default();
        let search_input_id = super::state::TextInputId::unique();

        let file_browser_show_hidden = addons.file_browser.show_hidden.unwrap_or(false);
        let file_browser_sort = addons.file_browser.sort;
        let file_browser_sort_reverse = addons.file_browser.sort_reverse;

        let fallbacks = Self::resolve_fallbacks(&fallback_paths, &addons);
        let snippet_mru_path = get_snippet_mru_cache_path().ok();
        let text_snippets = TextSnippetState {
            mru_map: load_mru_map(snippet_mru_path.as_deref()),
            mru_path: snippet_mru_path,
            ..Default::default()
        };

        (
            LauncherApp {
                filtered_configs: (0..configs.len()).collect(),
                configs,
                search_query: initial_query.clone(),
                selected_index: 0,
                selected_item,
                icon_map,
                no_icons,
                launcher_icon_paths,
                mru_map,
                sort_mode,
                addons,
                calculator_result: None,
                currency: CurrencyState::default(),
                script_filter: ScriptFilterState::default(),
                text_snippets,
                hyprland: None,
                deferred_paste: Default::default(),
                web_search: WebSearchState::default(),
                file_browser: FileBrowserState {
                    show_hidden: file_browser_show_hidden,
                    sort: file_browser_sort,
                    sort_reverse: file_browser_sort_reverse,
                    ..Default::default()
                },
                view: {
                    let mut view = ViewState::new(theme, font_sizes);
                    view.search_input_id = search_input_id.clone();
                    view
                },
                history: HistoryState {
                    items: load_history(max_history),
                    max_items: max_history,
                    ..Default::default()
                },
                emoji: EmojiState::default(),
                fallbacks,
                keyword_suggestions: Vec::new(),
                help_entries: Vec::new(),
                help_filtered: Vec::new(),
                help_saved_query: None,
                drag: None,
            },
            if initial_query.is_empty() {
                focus(search_input_id).chain(fetch_wayland_handles())
            } else {
                focus(search_input_id)
                    .chain(Task::done(Message::SearchChanged(initial_query)))
                    .chain(fetch_wayland_handles())
            },
        )
    }

    pub(super) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => self.handle_search_changed(query),
            Message::MoveUp => self.move_selection(-1),
            Message::MoveDown => self.move_selection(1),
            Message::Submit => self.activate_index(self.selected_index),
            Message::Cancel => {
                if self.view.help_active {
                    return self.close_help();
                }
                iced::exit()
            }
            Message::ItemClicked(index) => {
                self.selected_index = index;
                self.activate_index(index)
            }
            Message::CalculatorSelected => self.handle_calculator_selected(),
            Message::CurrencyConversionResult(request, result) => {
                self.handle_currency_conversion_result(request, result)
            }
            Message::CurrencyResultCopied => self.handle_currency_result_copied(),
            Message::MultiCurrencyConversionResult(request, result) => {
                self.handle_multi_currency_conversion_result(request, result)
            }
            Message::MultiCurrencyResultCopied(index) => {
                self.handle_multi_currency_result_copied(index)
            }
            Message::ScriptFilterResult(generation, result) => {
                self.handle_script_filter_result(generation, result)
            }
            Message::ScriptFilterItemSelected(index) => self.handle_script_filter_selected(index),
            Message::TextSnippetCommandResult(generation, result) => {
                self.handle_text_snippet_command_result(generation, result)
            }
            Message::TextSnippetSelected(index) => self.handle_text_snippet_selected(index),
            Message::TextSnippetClipboard(session, result) => {
                self.handle_text_snippet_clipboard(session, result)
            }
            Message::TextSnippetExpanded(session, result) => {
                self.handle_text_snippet_expanded(session, result)
            }
            Message::HyprPastePrepared(session, result) => {
                self.handle_hypr_paste_prepared(session, result)
            }
            Message::WebSearchSelected => self.handle_web_search_selected(),
            Message::FileBrowserItemSelected(index) => self.handle_file_browser_selected(index),
            Message::FileBrowserRowPressed { index, path } => {
                self.handle_file_browser_row_pressed(index, path)
            }
            Message::FileBrowserRowReleased(index) => self.handle_file_browser_row_released(index),
            Message::FileBrowserHover(row) => {
                self.file_browser.hovered_row = Some(row);
                Task::none()
            }
            Message::FileBrowserUnhover(row) => {
                if self.file_browser.hovered_row == Some(row) {
                    self.file_browser.hovered_row = None;
                }
                Task::none()
            }
            Message::MouseReleased => {
                // Every left release ends row press tracking, even the ones
                // that happen outside the file browser rows.
                self.file_browser.pressed_row = None;
                Task::none()
            }
            Message::WaylandHandles(handles) => self.handle_wayland_handles(handles),
            Message::DragEvent(event) => self.handle_drag_event(event),
            Message::TabComplete => self.handle_tab_complete(),
            Message::FileBrowserToggleHidden => self.handle_file_browser_toggle_hidden(),
            Message::FileBrowserOpenFileManager => self.handle_file_browser_open_file_manager(),
            Message::FileBrowserCycleSort => self.handle_file_browser_cycle_sort(),
            Message::ModifiersChanged(modifiers) => {
                self.view.current_modifiers = modifiers;
                Task::none()
            }
            Message::HistoryPrevious => self.handle_history_previous(),
            Message::HistoryNext => self.handle_history_next(),
            Message::ToggleHelp => self.toggle_help(),
            Message::TogglePreview => {
                // If a standard launcher is selected and launcher previews are currently hidden,
                // pressing Ctrl+P turns launcher preview on rather than disabling addon previews.
                if self.view.show_preview
                    && !self.view.show_launcher_preview
                    && self.resolve_selected_preview().is_none()
                {
                    self.view.show_launcher_preview = true;
                } else {
                    self.view.show_preview = !self.view.show_preview;
                    if self.view.show_preview {
                        self.view.show_launcher_preview = true;
                    }
                }
                self.refresh_file_preview_cache();
                Task::none()
            }
            Message::PreviewScrollDown => self.scroll_preview(PREVIEW_SCROLL_STEP),
            Message::PreviewScrollUp => self.scroll_preview(-PREVIEW_SCROLL_STEP),
            Message::WindowResized(size) => {
                self.view.window_size = size;
                Task::none()
            }
            Message::HelpEntrySelected(index) => self.handle_help_entry_selected(index),
            Message::EmojiDataLoaded(data) => self.handle_emoji_data_loaded(data),
            Message::EmojiSelected(index) => self.handle_emoji_selected(index),
            Message::FallbackSelected(index) => self.handle_fallback_selected(index),
            Message::KeywordSuggestionSelected(index) => {
                self.handle_keyword_suggestion_selected(index)
            }
        }
    }

    fn handle_search_changed(&mut self, query: String) -> Task<Message> {
        if self.view.current_modifiers.alt() && !self.history.search_in_progress {
            return Task::none();
        }

        let updir = if self.addons.file_browser.enabled
            && self.addons.file_browser.backspace_updir
            && self.file_browser.active
            && !self.history.search_in_progress
            && !self.view.help_active
        {
            fido_updir(
                &self.search_query,
                &query,
                &std::env::var("HOME").unwrap_or_default(),
            )
        } else {
            None
        };
        let (query, left_dir) = match updir {
            Some((parent, left)) => (parent, Some(left)),
            None => (query, None),
        };

        self.history.search_in_progress = false;

        if self.view.help_active {
            self.search_query = query;
            self.refresh_help_filter();
            self.view.refresh_ids();
            return Task::none();
        }

        self.search_query = query.clone();
        self.filter_items(&query);
        self.calculator_result = if self.addons.calculator.enabled {
            try_evaluate_math(&query)
        } else {
            None
        };
        self.selected_index = 0;

        if let Some(index) = self.history.index {
            if self.history.items.get(index).map(|h| h.as_str()) != Some(query.as_str()) {
                self.history.index = None;
            }
        }

        self.view.refresh_ids();

        let trimmed = query.trim();
        let mut tasks = Vec::new();

        let qmode = route_query(&query, &self.addons);
        crate::debug_log!("route_query: query={trimmed:?} -> {qmode:?}");
        self.keyword_suggestions = if matches!(qmode, QueryMode::Standard) {
            keyword_suggestions(&query, &self.addons)
        } else {
            Vec::new()
        };
        match qmode {
            QueryMode::FileBrowser { expanded, filter } => {
                self.handle_file_browser_query(&expanded, &filter);
                self.filtered_configs.clear();
                self.calculator_result = None;
                self.script_filter.clear();
                self.text_snippets.clear();
                self.emoji.clear();
                self.web_search.clear();
                self.currency.clear();
                let Some(left_dir) = left_dir else {
                    self.refresh_file_preview_cache();
                    return Task::batch(tasks);
                };
                if let Some(position) = self
                    .file_browser
                    .entries
                    .iter()
                    .position(|entry| entry.is_dir && entry.name == left_dir)
                {
                    self.selected_index = self.script_filter_offset() + position;
                }
                self.refresh_file_preview_cache();
                tasks.push(move_cursor_to_end(self.view.search_input_id.clone()));
                tasks.push(self.snap_to_selection());
                return Task::batch(tasks);
            }
            QueryMode::ScriptFilter {
                config_index,
                query,
            } => {
                self.file_browser.clear();
                self.handle_script_filter_query(config_index, query, &mut tasks);
                self.text_snippets.clear();
                self.emoji.clear();
                self.web_search.clear();
            }
            QueryMode::TextSnippet {
                config_index,
                query,
            } => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.handle_text_snippet_query(config_index, query, &mut tasks);
                self.emoji.clear();
                self.web_search.clear();
            }
            QueryMode::Emoji { query } => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.text_snippets.clear();
                self.handle_emoji_query(query, &mut tasks);
                self.web_search.clear();
            }
            QueryMode::WebSearch {
                config_index,
                query,
            } => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.text_snippets.clear();
                self.emoji.clear();
                self.handle_web_search_query(config_index, query);
            }
            QueryMode::Standard => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.text_snippets.clear();
                self.emoji.clear();
                self.web_search.clear();
            }
        }

        // The routing above left file-browser mode, so any drag armed by a
        // row press is stale (clear() already reset the row tracking).
        if !self.file_browser.active {
            if let Some(drag) = &self.drag {
                drag.disarm();
            }
        }

        if let Some(task) = self.handle_currency_query(&query, &mut tasks) {
            return task;
        }

        Task::batch(tasks)
    }

    fn handle_currency_query(
        &mut self,
        query: &str,
        tasks: &mut Vec<Task<Message>>,
    ) -> Option<Task<Message>> {
        let trigger = self.addons.currency.trigger.as_deref().unwrap_or("$");
        self.currency.help = self.addons.currency.enabled && is_currency_help_query(query, trigger);

        if self.currency.help {
            self.currency.clear_single();
            self.currency.clear_multi();
            self.currency.help = true;
            return Some(Task::batch(std::mem::take(tasks)));
        }

        if !self.addons.currency.enabled {
            self.currency.clear();
            return None;
        }

        let default_currency = self
            .addons
            .currency
            .default_currency
            .as_deref()
            .unwrap_or("USD");

        if let Some(request) = try_parse_currency_conversion(query, default_currency, trigger) {
            self.currency.clear_multi();

            let cache_key = format!("{}_{}", request.from_currency, request.to_currency);
            if let Some(cached) = self.currency.cache.get(&cache_key) {
                if cached.is_valid() {
                    self.currency.result = Some(CurrencyResult {
                        request: request.clone(),
                        converted_amount: request.amount * cached.rate,
                        rate: cached.rate,
                    });
                    self.currency.loading = false;
                    self.currency.error = None;
                    self.currency.pending_request = None;
                    return Some(Task::batch(std::mem::take(tasks)));
                }
            }

            self.currency.loading = true;
            self.currency.result = None;
            self.currency.error = None;
            self.currency.pending_request = Some(request.clone());
            tasks.push(fetch_exchange_rate(request));
            return Some(Task::batch(std::mem::take(tasks)));
        }

        let config_currencies = self.addons.currency.currencies.clone().unwrap_or_default();
        if let Some(request) = try_parse_multi_currency_conversion(
            query,
            &config_currencies,
            default_currency,
            trigger,
        ) {
            self.currency.clear_single();

            let mut all_cached = true;
            let mut conversions = Vec::new();
            for to_currency in &request.to_currencies {
                let cache_key = format!("{}_{}", request.from_currency, to_currency);
                if let Some(cached) = self.currency.cache.get(&cache_key) {
                    if cached.is_valid() {
                        conversions.push(CurrencyConversion {
                            to_currency: to_currency.clone(),
                            converted_amount: request.amount * cached.rate,
                            rate: cached.rate,
                        });
                    } else {
                        all_cached = false;
                        break;
                    }
                } else {
                    all_cached = false;
                    break;
                }
            }

            if all_cached && !conversions.is_empty() {
                self.currency.multi_result = Some(MultiCurrencyResult {
                    amount: request.amount,
                    from_currency: request.from_currency.clone(),
                    conversions,
                });
                self.currency.multi_loading = false;
                self.currency.pending_multi_request = None;
                return Some(Task::batch(std::mem::take(tasks)));
            }

            self.currency.multi_loading = true;
            self.currency.multi_result = None;
            self.currency.pending_multi_request = Some(request.clone());
            tasks.push(fetch_multi_exchange_rates(request));
            return Some(Task::batch(std::mem::take(tasks)));
        }

        self.currency.clear();
        None
    }

    fn handle_file_browser_query(&mut self, expanded: &str, filter_text: &str) {
        self.file_browser.active = true;
        self.file_browser.error = None;
        // Rows are about to be rebuilt; the pointer may now be over a
        // different one, so hover state from the previous listing is stale.
        self.file_browser.hovered_row = None;
        // Likewise the press: a row at the same index in the rebuilt listing
        // is a different file, so a held button must not activate or drag it.
        self.file_browser.pressed_row = None;
        if let Some(drag) = &self.drag {
            drag.disarm();
        }

        let dir_path = if expanded.ends_with('/') {
            expanded
        } else if let Some(last_slash) = expanded.rfind('/') {
            &expanded[..=last_slash]
        } else {
            expanded
        };

        if dir_path != self.file_browser.current_dir {
            self.file_browser.current_dir = dir_path.to_string();
            let all_entries = read_directory(
                dir_path,
                self.file_browser.show_hidden,
                self.file_browser.sort,
                self.file_browser.sort_reverse,
            );
            if all_entries.is_empty() && !Path::new(dir_path).is_dir() {
                self.file_browser.error = Some(format!("Cannot read directory: {}", dir_path));
            }
            self.file_browser.all_entries = all_entries;
        }

        if filter_text.is_empty() {
            self.file_browser.entries = self.file_browser.all_entries.clone();
        } else {
            let matcher = SkimMatcherV2::default();
            let mut scored: Vec<(usize, i64)> = self
                .file_browser
                .all_entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    matcher
                        .fuzzy_match(&entry.name, filter_text)
                        .map(|score| (index, score))
                })
                .collect();
            scored.sort_by_key(|b| std::cmp::Reverse(b.1));
            self.file_browser.entries = scored
                .into_iter()
                .map(|(index, _)| self.file_browser.all_entries[index].clone())
                .collect();
        }
    }

    fn handle_script_filter_query(
        &mut self,
        config_index: usize,
        query: String,
        tasks: &mut Vec<Task<Message>>,
    ) {
        if let Some(config) = self.addons.script_filters.get(config_index) {
            if let Some(min) = config.min_query_length {
                if query.len() < min {
                    self.script_filter.results = None;
                    self.script_filter.loading = false;
                    let remaining = min - query.len();
                    self.script_filter.help_message = Some(format!(
                        "Type at least {} more char{} to trigger {}",
                        remaining,
                        if remaining == 1 { "" } else { "s" },
                        config.name
                    ));
                    return;
                }
            }
            self.script_filter.help_message = None;
            self.filtered_configs.clear();
            self.script_filter.generation = self.script_filter.generation.wrapping_add(1);
            self.script_filter.loading = true;
            self.script_filter.loading_name = Some(config.name.clone());
            self.script_filter.action = config.action.clone();
            self.script_filter.secondary_action = config.secondary_action.clone();
            self.script_filter.results = None;
            tasks.push(execute_script_filter(
                config.command.clone(),
                config.args.clone(),
                config.env.clone(),
                query,
                self.script_filter.generation,
                config.icon.clone(),
            ));
        }
    }

    fn handle_text_snippet_query(
        &mut self,
        config_index: usize,
        query: String,
        tasks: &mut Vec<Task<Message>>,
    ) {
        let Some(config) = self.addons.text_snippets.get(config_index).cloned() else {
            return;
        };

        self.filtered_configs.clear();
        if !self.text_snippets.active || self.text_snippets.source_index != Some(config_index) {
            self.text_snippets.reset_session(Some(config_index));
        }
        self.text_snippets.active = true;
        self.text_snippets.icon = config.icon.clone();
        self.text_snippets.action = config.action.clone();
        self.text_snippets.secondary_action = config.secondary_action.clone();
        self.text_snippets.placeholders = config.placeholders;
        self.text_snippets.restore_clipboard = config.restore_clipboard;
        // Command output is not written by the user, so it may not run shell code.
        self.text_snippets.allow_shell = config.placeholders && config.command.is_none();
        self.text_snippets.error = None;

        if let Some(snippets) = config.snippets {
            self.text_snippets.items = snippets;
            self.finish_text_snippet_load(&query, tasks);
            return;
        }

        if let Some(file_path) = config.file {
            if let Some(cached) = self.text_snippets.file_cache.get(&file_path).cloned() {
                self.text_snippets.items = cached;
            } else {
                match fs::read_to_string(&file_path) {
                    Ok(contents) => {
                        match serde_yaml::from_str::<Vec<crate::TextSnippet>>(&contents) {
                            Ok(snippets) => {
                                self.text_snippets
                                    .file_cache
                                    .insert(file_path.clone(), snippets.clone());
                                self.text_snippets.items = snippets;
                            }
                            Err(error) => {
                                eprintln!(
                                    "Text snippets: invalid YAML in {}: {}",
                                    file_path, error
                                );
                                self.text_snippets.items = Vec::new();
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("Text snippets: cannot read {}: {}", file_path, error);
                        self.text_snippets.items = Vec::new();
                    }
                }
            }
            self.finish_text_snippet_load(&query, tasks);
            return;
        }

        if let Some(dir_path) = config.directory {
            if let Some(cached) = self.text_snippets.file_cache.get(&dir_path).cloned() {
                self.text_snippets.items = cached;
            } else {
                match fs::read_dir(&dir_path) {
                    Ok(entries) => {
                        let mut snippets = Vec::new();
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().and_then(|ext| ext.to_str()) != Some("snippet") {
                                continue;
                            }

                            if let Some(snippet) = fs::read_to_string(&path)
                                .ok()
                                .and_then(|contents| parse_snippet_file(&contents))
                            {
                                snippets.push(snippet);
                            }
                        }
                        snippets.sort_by(|a, b| a.name.cmp(&b.name));
                        self.text_snippets
                            .file_cache
                            .insert(dir_path.clone(), snippets.clone());
                        self.text_snippets.items = snippets;
                    }
                    Err(error) => {
                        eprintln!(
                            "Text snippets: cannot read directory {}: {}",
                            dir_path, error
                        );
                        self.text_snippets.items = Vec::new();
                    }
                }
            }
            self.finish_text_snippet_load(&query, tasks);
            return;
        }

        if let Some(command) = config.command {
            self.text_snippets.generation = self.text_snippets.generation.wrapping_add(1);
            self.text_snippets.loading = true;
            self.text_snippets.items.clear();
            self.text_snippets.filtered.clear();
            self.text_snippets.previews.clear();
            tasks.push(execute_text_snippet_command(
                command,
                config.args,
                self.text_snippets.generation,
            ));
        }
    }

    /// Filter freshly loaded snippets and refresh their previews.
    fn finish_text_snippet_load(&mut self, query: &str, tasks: &mut Vec<Task<Message>>) {
        let usage = self.text_snippet_usage();
        self.text_snippets.filtered = filter_snippets(&self.text_snippets.items, query, &usage);
        self.text_snippets.loading = false;
        if let Some(task) = self.refresh_text_snippet_previews() {
            tasks.push(task);
        }
    }

    /// `mru_map` key of `snippet` in the active source.
    fn text_snippet_mru_key(&self, snippet: &crate::TextSnippet) -> Option<String> {
        let source = self
            .text_snippets
            .source_index
            .and_then(|index| self.addons.text_snippets.get(index))?;
        Some(snippet_mru_key(&source.keyword, &snippet.name))
    }

    /// Usage score of every loaded snippet, indexed like `items`.
    fn text_snippet_usage(&self) -> Vec<u64> {
        let map = &self.text_snippets.mru_map;
        let (max_count, min_ts, max_ts) = mru_bounds(map);
        self.text_snippets
            .items
            .iter()
            .map(|snippet| {
                self.text_snippet_mru_key(snippet).map_or(0, |key| {
                    mru_sort_key(&key, map, &self.sort_mode, max_count, min_ts, max_ts)
                })
            })
            .collect()
    }

    /// Recompute snippet subtitles, starting a clipboard fetch the first time
    /// a snippet of this source needs it.
    fn refresh_text_snippet_previews(&mut self) -> Option<Task<Message>> {
        let state = &mut self.text_snippets;
        if !state.placeholders {
            state.previews.clear();
            return None;
        }
        let mut task = None;
        if !state.clipboard.fetching {
            let needs = missing_clipboard(&state.items, &state.clipboard.data);
            if !needs.is_empty() {
                state.clipboard.fetching = true;
                task = Some(fetch_clipboard(
                    state.session,
                    needs,
                    state.clipboard_history,
                ));
            }
        }
        state.previews = compute_previews(&state.items, state.allow_shell, &state.clipboard.data);
        task
    }

    fn handle_emoji_query(&mut self, query: String, tasks: &mut Vec<Task<Message>>) {
        if self.emoji.data.is_empty() && !self.emoji.data_loading {
            let file_names = resolve_emoji_file_names(&self.addons.emoji);
            let disk_data = load_emoji_data_from_disk(&file_names);
            if !disk_data.is_empty() {
                self.emoji.data = disk_data;
            } else {
                eprintln!("raffi: no local emoji data, using fallback while downloading...");
                self.emoji.data = emoji_fallback_entries();
                self.emoji.data_loading = true;
                tasks.push(Task::perform(
                    async move { download_and_load_emoji_data(file_names) },
                    Message::EmojiDataLoaded,
                ));
            }
        }

        self.filtered_configs.clear();
        self.emoji.active = true;
        self.emoji.action = self.addons.emoji.action.clone();
        self.emoji.secondary_action = self.addons.emoji.secondary_action.clone();
        filter_emoji_into(
            &self.emoji.data,
            &query,
            &self.emoji.matcher,
            &mut self.emoji.filtered,
        );
    }

    fn handle_web_search_query(&mut self, config_index: usize, query: String) {
        if let Some(config) = self.addons.web_searches.get(config_index) {
            self.filtered_configs.clear();
            self.web_search.active = Some(WebSearchActiveState {
                name: config.name.clone(),
                query,
                url_template: config.url.clone(),
                icon: config.icon.clone(),
            });
        }
    }

    /// Scroll the preview pane by `delta` pixels (negative scrolls up).
    /// A no-op when the pane is hidden; `scroll_by` clamps at the pane bounds.
    fn scroll_preview(&self, delta: f32) -> Task<Message> {
        if !self.view.show_preview {
            return Task::none();
        }
        scroll_by(
            self.view.preview_scrollable_id.clone(),
            AbsoluteOffset { x: 0.0, y: delta },
        )
    }

    fn move_selection(&mut self, delta: isize) -> Task<Message> {
        let total = self.total_items();
        if total == 0 {
            return Task::none();
        }

        if delta < 0 {
            if self.selected_index > 0 {
                self.selected_index -= 1;
            } else {
                self.selected_index = total - 1;
            }
        } else if self.selected_index < total.saturating_sub(1) {
            self.selected_index += 1;
        } else {
            self.selected_index = 0;
        }

        self.refresh_file_preview_cache();
        self.view.preview_scrollable_id = super::state::ScrollableId::unique();
        self.snap_to_selection()
    }

    fn snap_to_selection(&self) -> Task<Message> {
        let total = self.total_items();
        if total > 1 {
            let offset = self.selected_index as f32 / (total - 1) as f32;
            snap_to(
                self.view.scrollable_id.clone(),
                scrollable::RelativeOffset { x: 0.0, y: offset },
            )
        } else {
            Task::none()
        }
    }

    fn activate_index(&mut self, index: usize) -> Task<Message> {
        if self.view.help_active {
            return self.update(Message::HelpEntrySelected(index));
        }

        let mut current_index = 0;

        if index < self.keyword_suggestions.len() {
            return self.update(Message::KeywordSuggestionSelected(index));
        }
        current_index += self.keyword_suggestions.len();

        if self.script_filter.loading {
            if index == current_index {
                return Task::none();
            }
            current_index += 1;
        } else if let Some(result) = &self.script_filter.results {
            let item_count = result.items.len();
            if index >= current_index && index < current_index + item_count {
                return self.update(Message::ScriptFilterItemSelected(index - current_index));
            }
            current_index += item_count;
        }

        if self.text_snippets.loading {
            if index == current_index {
                return Task::none();
            }
            current_index += 1;
        } else if self.text_snippets.active {
            let item_count = self.text_snippets.filtered.len();
            if item_count > 0 && index >= current_index && index < current_index + item_count {
                return self.update(Message::TextSnippetSelected(index - current_index));
            }
            current_index += item_count;
        }

        if self.emoji.active {
            let item_count = self.emoji.filtered.len();
            if item_count > 0 && index >= current_index && index < current_index + item_count {
                return self.update(Message::EmojiSelected(index - current_index));
            }
            current_index += item_count;
        }

        if self.file_browser.active {
            let item_count = self.file_browser.entries.len();
            if item_count > 0 && index >= current_index && index < current_index + item_count {
                return self.update(Message::FileBrowserItemSelected(index - current_index));
            }
            current_index += item_count;
        }

        if self.web_search.active.is_some() {
            if index == current_index {
                return self.update(Message::WebSearchSelected);
            }
            current_index += 1;
        }

        if self.currency.loading {
            if index == current_index {
                return Task::none();
            }
            current_index += 1;
        } else if self.currency.result.is_some() {
            if index == current_index {
                return self.update(Message::CurrencyResultCopied);
            }
            current_index += 1;
        }

        if self.currency.multi_loading {
            if index == current_index {
                return Task::none();
            }
            current_index += 1;
        } else if let Some(result) = &self.currency.multi_result {
            let item_count = result.conversions.len();
            if index >= current_index && index < current_index + item_count {
                return self.update(Message::MultiCurrencyResultCopied(index - current_index));
            }
            current_index += item_count;
        }

        if self.calculator_result.is_some() {
            if index == current_index {
                return self.update(Message::CalculatorSelected);
            }
            current_index += 1;
        }

        let config_index = index.saturating_sub(current_index);
        if config_index < self.filtered_configs.len() {
            let real_config_index = self.filtered_configs[config_index];
            let config = &self.configs[real_config_index];
            let description = config
                .description
                .clone()
                .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
            if let Ok(mut selected) = self.selected_item.lock() {
                *selected = Some(description.clone());
            }
            record_mru_use(&mut self.mru_map, description);
            save_mru_map(get_mru_cache_path().ok().as_deref(), &self.mru_map);
            self.save_query_to_history();
            return iced::exit();
        }
        current_index += self.filtered_configs.len();

        let fallback_index = index.saturating_sub(current_index);
        if fallback_index < self.fallback_count() {
            return self.update(Message::FallbackSelected(fallback_index));
        }

        iced::exit()
    }

    fn handle_calculator_selected(&mut self) -> Task<Message> {
        if let Some(result) = &self.calculator_result {
            let formatted = if result.result.fract() == 0.0 {
                format!("{}", result.result as i64)
            } else {
                format!("{}", result.result)
            };
            spawn_copy(&formatted);
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_currency_conversion_result(
        &mut self,
        request: CurrencyConversionRequest,
        result: std::result::Result<CurrencyResult, String>,
    ) -> Task<Message> {
        if self.currency.pending_request.as_ref() != Some(&request) {
            return Task::none();
        }

        self.currency.loading = false;
        match result {
            Ok(currency_result) => {
                let cache_key = format!(
                    "{}_{}",
                    currency_result.request.from_currency, currency_result.request.to_currency
                );
                self.currency
                    .cache
                    .insert(cache_key, CachedRate::new(currency_result.rate));
                self.currency.result = Some(currency_result);
                self.currency.error = None;
            }
            Err(error) => {
                self.currency.result = None;
                self.currency.error = Some(error);
            }
        }
        self.currency.pending_request = None;
        Task::none()
    }

    fn handle_currency_result_copied(&mut self) -> Task<Message> {
        if let Some(currency) = &self.currency.result {
            spawn_copy(&format!("{:.2}", currency.converted_amount));
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_multi_currency_conversion_result(
        &mut self,
        request: MultiCurrencyRequest,
        result: std::result::Result<MultiCurrencyResult, String>,
    ) -> Task<Message> {
        if self.currency.pending_multi_request.as_ref() != Some(&request) {
            return Task::none();
        }

        self.currency.multi_loading = false;
        match result {
            Ok(multi_result) => {
                for conversion in &multi_result.conversions {
                    let cache_key =
                        format!("{}_{}", multi_result.from_currency, conversion.to_currency);
                    self.currency
                        .cache
                        .insert(cache_key, CachedRate::new(conversion.rate));
                }
                self.currency.multi_result = Some(multi_result);
            }
            Err(_) => {
                self.currency.multi_result = None;
            }
        }
        self.currency.pending_multi_request = None;
        Task::none()
    }

    fn handle_multi_currency_result_copied(&mut self, index: usize) -> Task<Message> {
        if let Some(result) = &self.currency.multi_result {
            if let Some(conversion) = result.conversions.get(index) {
                spawn_copy(&format!("{:.2}", conversion.converted_amount));
            }
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_script_filter_result(
        &mut self,
        generation: u64,
        result: std::result::Result<ScriptFilterResult, String>,
    ) -> Task<Message> {
        if generation != self.script_filter.generation {
            return Task::none();
        }

        self.script_filter.loading = false;
        self.script_filter.loading_name = None;
        self.script_filter.results = result.ok();
        self.refresh_file_preview_cache();
        Task::none()
    }

    fn handle_script_filter_selected(&mut self, index: usize) -> Task<Message> {
        if let Some(result) = &self.script_filter.results {
            if let Some(item) = result.items.get(index) {
                let value = item.arg.as_deref().unwrap_or(&item.title);
                let action = if self.view.current_modifiers.control() {
                    self.script_filter
                        .secondary_action
                        .as_deref()
                        .or(self.script_filter.action.as_deref())
                } else {
                    self.script_filter.action.as_deref()
                };
                execute_action(action.unwrap_or("copy"), value);
            }
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_text_snippet_command_result(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<crate::TextSnippet>, String>,
    ) -> Task<Message> {
        // A late result after leaving snippet mode must not reload snippets or
        // start a clipboard read.
        if !self.text_snippets.active || generation != self.text_snippets.generation {
            return Task::none();
        }

        self.text_snippets.loading = false;
        match result {
            Ok(snippets) => {
                self.text_snippets.items = snippets;
                let input = keyword_input(&self.search_query);
                let query = self
                    .addons
                    .text_snippets
                    .iter()
                    .find_map(|config| extract_keyword_query(input, &config.keyword))
                    .unwrap_or_default();
                let mut tasks = Vec::new();
                self.finish_text_snippet_load(&query, &mut tasks);
                return Task::batch(tasks);
            }
            Err(_) => {
                self.text_snippets.items.clear();
                self.text_snippets.filtered.clear();
                self.text_snippets.previews.clear();
            }
        }
        Task::none()
    }

    fn handle_text_snippet_clipboard(
        &mut self,
        session: u64,
        data: ClipboardData,
    ) -> Task<Message> {
        if !self.text_snippets.active || session != self.text_snippets.session {
            return Task::none();
        }
        self.text_snippets.clipboard.fetching = false;
        self.text_snippets.clipboard.data.merge(data);
        self.refresh_text_snippet_previews()
            .unwrap_or_else(Task::none)
    }

    fn handle_text_snippet_selected(&mut self, index: usize) -> Task<Message> {
        if self.text_snippets.pending.is_some() {
            return Task::none();
        }
        let snippet = self
            .text_snippets
            .filtered
            .get(index)
            .and_then(|&snippet_index| self.text_snippets.items.get(snippet_index))
            .cloned();
        if let Some(snippet) = snippet {
            if let Some(key) = self.text_snippet_mru_key(&snippet) {
                let state = &mut self.text_snippets;
                record_mru_use(&mut state.mru_map, key);
                save_mru_map(state.mru_path.as_deref(), &state.mru_map);
            }
            let action = if self.view.current_modifiers.control() {
                self.text_snippets
                    .secondary_action
                    .as_deref()
                    .unwrap_or("insert")
            } else {
                self.text_snippets.action.as_deref().unwrap_or("copy")
            }
            .to_string();

            if self.text_snippets.placeholders {
                let state = &mut self.text_snippets;
                state.error = None;
                state.pending = Some(PendingSelection {
                    action,
                    preparing_paste: false,
                });
                return expand_final(
                    snippet.value,
                    state.items.clone(),
                    state.allow_shell,
                    state.session,
                    state.clipboard_history,
                );
            }
            if action == "hypr_paste" {
                return self.prepare_hypr_paste(snippet.value);
            }
            execute_action(&action, &snippet.value);
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_text_snippet_expanded(
        &mut self,
        session: u64,
        result: std::result::Result<super::placeholders::Expanded, String>,
    ) -> Task<Message> {
        let state = &mut self.text_snippets;
        if !state.active || session != state.session {
            return Task::none();
        }
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.preparing_paste)
        {
            return Task::none();
        }
        let Some(pending) = state.pending.take() else {
            return Task::none();
        };
        if pending.action == "hypr_paste" {
            return match result {
                Ok(expanded) => self.prepare_hypr_paste(expanded.text),
                Err(error) => {
                    state.error = Some(error);
                    Task::none()
                }
            };
        }
        let outcome = result.and_then(|expanded| {
            execute_snippet_action(&pending.action, &expanded.text, expanded.cursor_moves)
        });
        match outcome {
            Ok(()) => {
                self.save_query_to_history();
                iced::exit()
            }
            Err(error) => {
                crate::debug_log!("snippets: selection failed: {error}");
                state.error = Some(error);
                Task::none()
            }
        }
    }

    fn prepare_hypr_paste(&mut self, value: String) -> Task<Message> {
        let target = self.hyprland.clone().unwrap_or_else(|| {
            Err("hypr_paste requires a Hyprland session and an original target window".into())
        });
        let mut target = match target {
            Ok(target) => target,
            Err(error) => {
                self.text_snippets.error = Some(error);
                return Task::none();
            }
        };
        let state = &mut self.text_snippets;
        state.error = None;
        state.pending = Some(PendingSelection {
            action: "hypr_paste".into(),
            preparing_paste: true,
        });
        let session = state.session;
        let restore_clipboard = state.restore_clipboard;
        Task::perform(
            async move { target.prepare(&value, restore_clipboard).map(|()| target) },
            move |result| Message::HyprPastePrepared(session, result),
        )
    }

    fn handle_hypr_paste_prepared(
        &mut self,
        session: u64,
        result: Result<super::hyprland::Hyprland, String>,
    ) -> Task<Message> {
        let state = &mut self.text_snippets;
        if !state.active
            || session != state.session
            || !state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.preparing_paste)
        {
            return Task::none();
        }
        state.pending = None;
        let outcome = result.and_then(|target| {
            self.deferred_paste
                .lock()
                .map_err(|_| "cannot queue Hyprland paste".to_string())?
                .replace(target);
            Ok(())
        });
        match outcome {
            Ok(()) => {
                self.save_query_to_history();
                iced::exit()
            }
            Err(error) => {
                crate::debug_log!("hypr_paste preparation failed: {error}");
                state.error = Some(error);
                Task::none()
            }
        }
    }

    fn handle_web_search_selected(&mut self) -> Task<Message> {
        if let Some(web_search) = &self.web_search.active {
            if !web_search.query.is_empty() {
                let _ = crate::execute_web_search_url(&web_search.url_template, &web_search.query);
            }
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_file_browser_row_pressed(&mut self, index: usize, path: String) -> Task<Message> {
        crate::debug_log!("browser: row {index} pressed, arming drag for {path:?}");
        self.file_browser.pressed_row = Some(index);
        if let Some(drag) = &self.drag {
            drag.arm(path);
        }
        Task::none()
    }

    fn handle_file_browser_row_released(&mut self, index: usize) -> Task<Message> {
        let was_pressed = self.file_browser.pressed_row == Some(index);
        self.file_browser.pressed_row = None;
        if let Some(drag) = &self.drag {
            drag.disarm();
        }
        if !was_pressed {
            // The press started elsewhere (or was consumed by a drag), so
            // this release must not activate the row.
            return Task::none();
        }
        self.update(Message::ItemClicked(index))
    }

    fn handle_wayland_handles(
        &mut self,
        handles: Option<super::dnd::RawWaylandHandles>,
    ) -> Task<Message> {
        let Some(handles) = handles else {
            crate::debug_log!("dnd: no Wayland window, drag-and-drop disabled");
            return Task::none();
        };
        let (source, events) = super::dnd::spawn(handles);
        self.drag = Some(source);
        use iced::futures::StreamExt;
        Task::stream(events.map(Message::DragEvent))
    }

    fn handle_drag_event(&mut self, event: super::dnd::DragEvent) -> Task<Message> {
        crate::debug_log!("dnd: session event: {event:?}");
        match event {
            super::dnd::DragEvent::Started => {
                self.file_browser.dragging = true;
                // The compositor grabbed the pointer: the matching release
                // will never arrive, so press tracking has to reset now.
                self.file_browser.pressed_row = None;
                self.file_browser.hovered_row = None;
                Task::none()
            }
            super::dnd::DragEvent::Cancelled => {
                self.file_browser.dragging = false;
                Task::none()
            }
            super::dnd::DragEvent::Dropped => {
                self.file_browser.dragging = false;
                self.save_query_to_history();
                iced::exit()
            }
        }
    }

    fn handle_file_browser_selected(&mut self, index: usize) -> Task<Message> {
        if let Some(entry) = self.file_browser.entries.get(index) {
            let secondary = self.view.current_modifiers.control();
            if entry.is_dir && !secondary {
                let new_query = format!("{}/", entry.full_path);
                self.search_query = new_query.clone();
                return Task::done(Message::SearchChanged(new_query));
            }

            let config = &self.addons.file_browser;
            let action = if secondary {
                config.secondary_action()
            } else {
                config.action()
            };
            execute_file_action(action, &entry.full_path);
        }

        self.save_query_to_history();
        iced::exit()
    }

    fn handle_tab_complete(&mut self) -> Task<Message> {
        if self.view.help_active {
            return self.update(Message::HelpEntrySelected(self.selected_index));
        }

        if self.selected_index < self.keyword_suggestions.len() {
            return self.update(Message::KeywordSuggestionSelected(self.selected_index));
        }

        if !self.file_browser.active {
            return Task::none();
        }

        let entry_index = self
            .selected_index
            .saturating_sub(self.script_filter_offset());
        if let Some(entry) = self.file_browser.entries.get(entry_index) {
            let new_query = if entry.is_dir {
                format!("{}/", entry.full_path)
            } else {
                entry.full_path.clone()
            };
            self.search_query = new_query.clone();
            let input_id = self.view.search_input_id.clone();
            return Task::done(Message::SearchChanged(new_query))
                .chain(move_cursor_to_end(input_id));
        }

        Task::none()
    }

    fn handle_file_browser_toggle_hidden(&mut self) -> Task<Message> {
        self.file_browser.show_hidden = !self.file_browser.show_hidden;
        if self.file_browser.active && !self.file_browser.current_dir.is_empty() {
            self.file_browser.all_entries = read_directory(
                &self.file_browser.current_dir,
                self.file_browser.show_hidden,
                self.file_browser.sort,
                self.file_browser.sort_reverse,
            );
            let query = self.search_query.clone();
            return Task::done(Message::SearchChanged(query));
        }
        Task::none()
    }

    fn handle_file_browser_cycle_sort(&mut self) -> Task<Message> {
        if !self.file_browser.active {
            return Task::none();
        }
        self.file_browser.sort = self.file_browser.sort.next();
        if !self.file_browser.current_dir.is_empty() {
            self.file_browser.all_entries = read_directory(
                &self.file_browser.current_dir,
                self.file_browser.show_hidden,
                self.file_browser.sort,
                self.file_browser.sort_reverse,
            );
        }
        let query = self.search_query.clone();
        Task::done(Message::SearchChanged(query))
    }

    fn handle_file_browser_open_file_manager(&mut self) -> Task<Message> {
        let dir = &self.file_browser.current_dir;
        if !self.file_browser.active || dir.is_empty() || !Path::new(dir).is_dir() {
            return Task::none();
        }
        let entry_index = self
            .selected_index
            .saturating_sub(self.script_filter_offset());
        let selected = self
            .file_browser
            .entries
            .get(entry_index)
            .map(|entry| entry.full_path.as_str());
        open_in_file_manager(
            dir,
            selected,
            self.addons.file_browser.file_manager.as_deref(),
        );
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_history_previous(&mut self) -> Task<Message> {
        if self.history.items.is_empty() || self.history.max_items == 0 {
            return Task::none();
        }

        match self.history.index {
            None => {
                self.history.saved_query = self.search_query.clone();
                self.history.index = Some(self.history.items.len() - 1);
            }
            Some(0) => return Task::none(),
            Some(index) => self.history.index = Some(index - 1),
        }

        let query = self.history.items[self.history.index.unwrap()].clone();
        let input_id = self.view.search_input_id.clone();
        self.history.search_in_progress = true;
        self.update(Message::SearchChanged(query))
            .chain(move_cursor_to_end(input_id))
    }

    fn handle_history_next(&mut self) -> Task<Message> {
        if self.history.max_items == 0 {
            return Task::none();
        }

        match self.history.index {
            None => Task::none(),
            Some(index) => {
                if index + 1 >= self.history.items.len() {
                    self.history.index = None;
                    let query = self.history.saved_query.clone();
                    let input_id = self.view.search_input_id.clone();
                    self.history.search_in_progress = true;
                    self.update(Message::SearchChanged(query))
                        .chain(move_cursor_to_end(input_id))
                } else {
                    self.history.index = Some(index + 1);
                    let query = self.history.items[index + 1].clone();
                    let input_id = self.view.search_input_id.clone();
                    self.history.search_in_progress = true;
                    self.update(Message::SearchChanged(query))
                        .chain(move_cursor_to_end(input_id))
                }
            }
        }
    }

    fn handle_emoji_data_loaded(&mut self, data: Vec<EmojiEntry>) -> Task<Message> {
        self.emoji.data_loading = false;
        if !data.is_empty() {
            self.emoji.data = data;
            if self.emoji.active {
                let emoji_trigger = self.addons.emoji.trigger.as_deref().unwrap_or("emoji");
                let input = keyword_input(&self.search_query);
                let emoji_query = extract_keyword_query(input, emoji_trigger).unwrap_or_default();
                filter_emoji_into(
                    &self.emoji.data,
                    &emoji_query,
                    &self.emoji.matcher,
                    &mut self.emoji.filtered,
                );
            }
        }
        Task::none()
    }

    fn handle_emoji_selected(&mut self, index: usize) -> Task<Message> {
        if let Some(&emoji_index) = self.emoji.filtered.get(index) {
            if let Some(entry) = self.emoji.data.get(emoji_index) {
                let action = if self.view.current_modifiers.control() {
                    self.emoji.secondary_action.as_deref().unwrap_or("insert")
                } else {
                    self.emoji.action.as_deref().unwrap_or("copy")
                };
                execute_action(action, &entry.value);
            }
        }
        self.save_query_to_history();
        iced::exit()
    }

    fn script_filter_offset(&self) -> usize {
        if self.script_filter.loading {
            1
        } else {
            self.script_filter
                .results
                .as_ref()
                .map(|result| result.items.len())
                .unwrap_or(0)
        }
    }

    pub(super) fn filter_items(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_configs = (0..self.configs.len()).collect();
        } else {
            self.filtered_configs = fuzzy_match_configs(&self.configs, query);
        }
    }

    pub(super) fn total_items(&self) -> usize {
        if self.view.help_active {
            return self.help_filtered.len();
        }
        let mut offset = self.keyword_suggestions.len();
        if self.script_filter.loading {
            offset += 1;
        } else if let Some(result) = &self.script_filter.results {
            offset += result.items.len();
        }
        if self.text_snippets.loading {
            offset += 1;
        } else if self.text_snippets.active {
            offset += self.text_snippets.filtered.len();
        }
        if self.emoji.active {
            offset += self.emoji.filtered.len();
        }
        if self.file_browser.active {
            offset += self.file_browser.entries.len();
        }
        if self.web_search.active.is_some() {
            offset += 1;
        }
        if self.currency.help {
            offset += 1;
        }
        if self.currency.result.is_some() || self.currency.loading {
            offset += 1;
        }
        if self.currency.multi_loading {
            offset += 1;
        } else if let Some(result) = &self.currency.multi_result {
            offset += result.conversions.len();
        }
        if self.calculator_result.is_some() {
            offset += 1;
        }
        self.filtered_configs.len() + offset + self.fallback_count()
    }

    fn is_in_addon_mode(&self) -> bool {
        self.script_filter.results.is_some()
            || self.script_filter.loading
            || self.text_snippets.active
            || self.text_snippets.loading
            || self.emoji.active
            || self.file_browser.active
            || self.web_search.active.is_some()
    }

    pub(super) fn fallback_count(&self) -> usize {
        if !self.view.help_active
            && !self.search_query.trim().is_empty()
            && !self.is_in_addon_mode()
            && self.keyword_suggestions.is_empty()
        {
            self.fallbacks.len()
        } else {
            0
        }
    }

    fn resolve_fallbacks(paths: &[String], addons: &AddonsConfig) -> Vec<ResolvedFallback> {
        let mut resolved = Vec::new();
        for path in paths {
            let parts: Vec<&str> = path.splitn(3, '.').collect();
            if parts.len() != 3 || parts[0] != "addons" {
                crate::debug_log!("fallback: invalid path '{path}', expected addons.<type>.<name>");
                eprintln!("Warning: invalid fallback path '{path}'");
                continue;
            }
            let addon_type = parts[1];
            let name = parts[2].trim_matches('"');
            match addon_type {
                "web_searches" => {
                    if let Some(ws) = addons.web_searches.iter().find(|w| w.name == name) {
                        resolved.push(ResolvedFallback {
                            name: ws.name.clone(),
                            icon: ws.icon.clone(),
                            action: FallbackAction::WebSearch {
                                url_template: ws.url.clone(),
                            },
                        });
                    } else {
                        crate::debug_log!("fallback: web search '{name}' not found");
                        eprintln!("Warning: fallback web search '{name}' not found in addons");
                    }
                }
                "script_filters" => {
                    if let Some(sf) = addons.script_filters.iter().find(|s| s.name == name) {
                        resolved.push(ResolvedFallback {
                            name: sf.name.clone(),
                            icon: sf.icon.clone(),
                            action: FallbackAction::ScriptFilter {
                                keyword: sf.keyword.clone(),
                                command: sf.command.clone(),
                                args: sf.args.clone(),
                                action: sf.action.clone(),
                                secondary_action: sf.secondary_action.clone(),
                            },
                        });
                    } else {
                        crate::debug_log!("fallback: script filter '{name}' not found");
                        eprintln!("Warning: fallback script filter '{name}' not found in addons");
                    }
                }
                _ => {
                    crate::debug_log!(
                        "fallback: unsupported addon type '{addon_type}', expected web_searches or script_filters"
                    );
                    eprintln!("Warning: unsupported fallback addon type '{addon_type}'");
                }
            }
        }
        resolved
    }

    fn handle_fallback_selected(&mut self, index: usize) -> Task<Message> {
        if let Some(fallback) = self.fallbacks.get(index).cloned() {
            let query = self.search_query.trim().to_string();
            match &fallback.action {
                FallbackAction::WebSearch { url_template } => {
                    if !query.is_empty() {
                        let _ = crate::execute_web_search_url(url_template, &query);
                    }
                    self.save_query_to_history();
                    return iced::exit();
                }
                FallbackAction::ScriptFilter {
                    command,
                    args,
                    action,
                    ..
                } => {
                    if !query.is_empty() {
                        let output = std::process::Command::new(command)
                            .args(args)
                            .stdin(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .arg(&query)
                            .output();
                        if let Ok(output) = output {
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            if let Ok(result) = parse_script_filter_output(&stdout, None) {
                                if let Some(first) = result.items.first() {
                                    let value =
                                        first.arg.as_deref().unwrap_or(first.title.as_str());
                                    let act = action.as_deref().unwrap_or("copy");
                                    execute_action(act, value);
                                }
                            }
                        }
                    }
                    self.save_query_to_history();
                    return iced::exit();
                }
            }
        }
        Task::none()
    }

    /// Toggle the `Ctrl+/` keyword help list.
    ///
    /// Opening it suppresses launcher entries and every addon result so the
    /// list shows nothing but help rows; closing it replays the current query
    /// through the normal routing.
    fn toggle_help(&mut self) -> Task<Message> {
        if self.view.help_active {
            return self.close_help();
        }

        self.help_saved_query = Some(std::mem::take(&mut self.search_query));
        self.view.help_active = true;
        if self.help_entries.is_empty() {
            self.help_entries = help_entries(&self.addons);
        }
        self.filtered_configs.clear();
        self.keyword_suggestions.clear();
        self.calculator_result = None;
        self.script_filter.clear();
        self.text_snippets.clear();
        self.emoji.clear();
        self.web_search.clear();
        self.file_browser.clear();
        // The browser is gone; a drag armed by a row press is stale.
        if let Some(drag) = &self.drag {
            drag.disarm();
        }
        self.currency.clear();
        self.refresh_help_filter();
        self.view.refresh_ids();
        crate::debug_log!("help: opened with {} entries", self.help_filtered.len());
        Task::none()
    }

    fn close_help(&mut self) -> Task<Message> {
        let query = self
            .help_saved_query
            .take()
            .expect("help saves the search query when it opens");
        self.view.help_active = false;
        self.help_filtered.clear();
        crate::debug_log!("help: closed");
        self.handle_search_changed(query)
    }

    fn refresh_help_filter(&mut self) {
        self.help_filtered = filter_help_entries(&self.help_entries, &self.search_query);
        self.selected_index = 0;
    }

    fn handle_help_entry_selected(&mut self, index: usize) -> Task<Message> {
        let Some(entry) = self.help_filtered.get(index).and_then(|&entry_index| {
            self.help_entries
                .get(entry_index)
                .and_then(|entry| entry.completion.clone())
        }) else {
            return Task::none();
        };

        crate::debug_log!("help: arming {entry:?}");
        self.view.help_active = false;
        self.help_filtered.clear();
        self.help_saved_query = None;
        let input_id = self.view.search_input_id.clone();
        self.update(Message::SearchChanged(entry))
            .chain(move_cursor_to_end(input_id))
    }

    fn handle_keyword_suggestion_selected(&mut self, index: usize) -> Task<Message> {
        let Some(suggestion) = self.keyword_suggestions.get(index) else {
            return Task::none();
        };

        let new_query = format!("{} ", suggestion.keyword);
        crate::debug_log!("keyword suggestion: completing to {new_query:?}");
        let input_id = self.view.search_input_id.clone();
        self.update(Message::SearchChanged(new_query))
            .chain(move_cursor_to_end(input_id))
    }

    pub(super) fn save_query_to_history(&mut self) {
        let query = self.search_query.trim().to_string();
        if !query.is_empty() && self.history.max_items > 0 {
            self.history.items.retain(|item| item != &query);
            self.history.items.push(query);
            if self.history.items.len() > self.history.max_items as usize {
                let excess = self.history.items.len() - self.history.max_items as usize;
                self.history.items.drain(..excess);
            }
            save_history(&self.history.items);
        }
    }
}
