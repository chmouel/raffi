use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, move_cursor_to_end, snap_to};
use iced::widget::scrollable;
use iced::Task;

use super::actions::{execute_action, spawn_copy};
use super::browser::read_directory;
use super::currency::{
    fetch_exchange_rate, fetch_multi_exchange_rates, is_currency_help_query,
    try_parse_currency_conversion, try_parse_multi_currency_conversion,
};
use super::emoji::{
    download_and_load_emoji_data, emoji_fallback_entries, filter_emoji_into,
    load_emoji_data_from_disk, resolve_emoji_file_names,
};
use super::script_filters::execute_script_filter;
use super::snippets::{execute_text_snippet_command, filter_snippets};
use super::state::{
    CachedRate, CurrencyConversion, CurrencyConversionRequest, CurrencyResult, CurrencyState,
    EmojiEntry, EmojiState, FileBrowserState, HistoryState, LauncherApp, Message,
    MultiCurrencyRequest, MultiCurrencyResult, ScriptFilterResult, ScriptFilterState,
    SharedSelection, TextSnippetState, ViewState, WebSearchActiveState, WebSearchState,
};
use super::support::{
    fuzzy_match_configs, load_history, load_mru_map, mru_sort_key, save_history, save_mru_map,
    try_evaluate_math, MruEntry,
};
use crate::ui::FontSizes;
use crate::{read_icon_map, AddonsConfig, RaffiConfig, SortMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum QueryMode {
    FileBrowser { expanded: String, filter: String },
    ScriptFilter { config_index: usize, query: String },
    TextSnippet { config_index: usize, query: String },
    Emoji { query: String },
    WebSearch { config_index: usize, query: String },
    Standard,
}

fn extract_keyword_query(trimmed: &str, keyword: &str) -> Option<String> {
    if trimmed == keyword {
        return Some(String::new());
    }

    trimmed
        .strip_prefix(keyword)
        .and_then(|rest| rest.strip_prefix(' '))
        .map(ToOwned::to_owned)
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

pub(super) fn route_query(trimmed: &str, addons: &AddonsConfig) -> QueryMode {
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
                extract_keyword_query(trimmed, &config.keyword).map(|query| (index, query))
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
                extract_keyword_query(trimmed, &config.keyword).map(|query| (index, query))
            })
    {
        return QueryMode::TextSnippet {
            config_index,
            query,
        };
    }

    let emoji_trigger = addons.emoji.trigger.as_deref().unwrap_or("emoji");
    if addons.emoji.enabled {
        if let Some(query) = extract_keyword_query(trimmed, emoji_trigger) {
            return QueryMode::Emoji { query };
        }
    }

    if let Some((config_index, query)) =
        addons
            .web_searches
            .iter()
            .enumerate()
            .find_map(|(index, config)| {
                extract_keyword_query(trimmed, &config.keyword).map(|query| (index, query))
            })
    {
        return QueryMode::WebSearch {
            config_index,
            query,
        };
    }

    QueryMode::Standard
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

        let mru_map = load_mru_map();

        // Pre-compute aggregate stats for hybrid sorting
        let max_count = mru_map.values().map(|e| e.count).max().unwrap_or(0);
        let min_ts = mru_map
            .values()
            .map(|e| e.last_used)
            .filter(|&t| t > 0)
            .min()
            .unwrap_or(0);
        let max_ts = mru_map.values().map(|e| e.last_used).max().unwrap_or(0);

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

        let initial_query = initial_query.unwrap_or_default();
        let search_input_id = super::state::TextInputId::unique();

        let file_browser_show_hidden = addons.file_browser.show_hidden.unwrap_or(false);

        (
            LauncherApp {
                filtered_configs: (0..configs.len()).collect(),
                configs,
                search_query: initial_query.clone(),
                selected_index: 0,
                selected_item,
                icon_map,
                mru_map,
                addons,
                calculator_result: None,
                currency: CurrencyState::default(),
                script_filter: ScriptFilterState::default(),
                text_snippets: TextSnippetState::default(),
                web_search: WebSearchState::default(),
                file_browser: FileBrowserState {
                    show_hidden: file_browser_show_hidden,
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
            },
            if initial_query.is_empty() {
                focus(search_input_id)
            } else {
                focus(search_input_id).chain(Task::done(Message::SearchChanged(initial_query)))
            },
        )
    }

    pub(super) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => self.handle_search_changed(query),
            Message::MoveUp => self.move_selection(-1),
            Message::MoveDown => self.move_selection(1),
            Message::Submit => self.activate_index(self.selected_index),
            Message::Cancel => iced::exit(),
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
            Message::WebSearchSelected => self.handle_web_search_selected(),
            Message::FileBrowserItemSelected(index) => self.handle_file_browser_selected(index),
            Message::FileBrowserTabComplete => self.handle_file_browser_tab_complete(),
            Message::FileBrowserToggleHidden => self.handle_file_browser_toggle_hidden(),
            Message::ModifiersChanged(modifiers) => {
                self.view.current_modifiers = modifiers;
                Task::none()
            }
            Message::HistoryPrevious => self.handle_history_previous(),
            Message::HistoryNext => self.handle_history_next(),
            Message::ToggleHints => {
                self.view.show_hints = !self.view.show_hints;
                Task::none()
            }
            Message::EmojiDataLoaded(data) => self.handle_emoji_data_loaded(data),
            Message::EmojiSelected(index) => self.handle_emoji_selected(index),
        }
    }

    fn handle_search_changed(&mut self, query: String) -> Task<Message> {
        if self.view.current_modifiers.alt() && !self.history.search_in_progress {
            return Task::none();
        }

        self.history.search_in_progress = false;
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

        let qmode = route_query(trimmed, &self.addons);
        crate::debug_log!("route_query: query={trimmed:?} -> {qmode:?}");
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

        let dir_path = if expanded.ends_with('/') {
            expanded
        } else if let Some(last_slash) = expanded.rfind('/') {
            &expanded[..=last_slash]
        } else {
            expanded
        };

        if dir_path != self.file_browser.current_dir {
            self.file_browser.current_dir = dir_path.to_string();
            let all_entries = read_directory(dir_path, self.file_browser.show_hidden);
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
            scored.sort_by(|a, b| b.1.cmp(&a.1));
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
        self.text_snippets.active = true;
        self.text_snippets.icon = config.icon.clone();
        self.text_snippets.action = config.action.clone();
        self.text_snippets.secondary_action = config.secondary_action.clone();

        if let Some(snippets) = config.snippets {
            self.text_snippets.items = snippets;
            self.text_snippets.filtered = filter_snippets(&self.text_snippets.items, &query);
            self.text_snippets.loading = false;
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
            self.text_snippets.filtered = filter_snippets(&self.text_snippets.items, &query);
            self.text_snippets.loading = false;
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

                            if let Ok(contents) = fs::read_to_string(&path) {
                                let mut lines = contents.lines();
                                if let Some(name) = lines.next() {
                                    if let Some(separator) = lines.next() {
                                        if separator.trim() == "---" {
                                            let value = lines.collect::<Vec<_>>().join("\n");
                                            if !value.is_empty() {
                                                snippets.push(crate::TextSnippet {
                                                    name: name.trim().to_string(),
                                                    value,
                                                });
                                            }
                                        }
                                    }
                                }
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
            self.text_snippets.filtered = filter_snippets(&self.text_snippets.items, &query);
            self.text_snippets.loading = false;
            return;
        }

        if let Some(command) = config.command {
            self.text_snippets.generation = self.text_snippets.generation.wrapping_add(1);
            self.text_snippets.loading = true;
            self.text_snippets.items.clear();
            self.text_snippets.filtered.clear();
            tasks.push(execute_text_snippet_command(
                command,
                config.args,
                self.text_snippets.generation,
            ));
        }
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
        let mut current_index = 0;

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
        if let Some(&config_index) = self.filtered_configs.get(config_index) {
            let config = &self.configs[config_index];
            let description = config
                .description
                .clone()
                .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
            if let Ok(mut selected) = self.selected_item.lock() {
                *selected = Some(description.clone());
            }
            let entry = self.mru_map.entry(description).or_insert(MruEntry {
                count: 0,
                last_used: 0,
            });
            entry.count += 1;
            entry.last_used = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            save_mru_map(&self.mru_map);
            self.save_query_to_history();
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
        if generation != self.text_snippets.generation {
            return Task::none();
        }

        self.text_snippets.loading = false;
        match result {
            Ok(snippets) => {
                self.text_snippets.items = snippets;
                let trimmed = self.search_query.trim();
                let query = self
                    .addons
                    .text_snippets
                    .iter()
                    .find_map(|config| extract_keyword_query(trimmed, &config.keyword))
                    .unwrap_or_default();
                self.text_snippets.filtered = filter_snippets(&self.text_snippets.items, &query);
            }
            Err(_) => {
                self.text_snippets.items.clear();
                self.text_snippets.filtered.clear();
            }
        }
        Task::none()
    }

    fn handle_text_snippet_selected(&mut self, index: usize) -> Task<Message> {
        if let Some(&snippet_index) = self.text_snippets.filtered.get(index) {
            if let Some(snippet) = self.text_snippets.items.get(snippet_index) {
                let action = if self.view.current_modifiers.control() {
                    self.text_snippets
                        .secondary_action
                        .as_deref()
                        .unwrap_or("insert")
                } else {
                    self.text_snippets.action.as_deref().unwrap_or("copy")
                };
                execute_action(action, &snippet.value);
            }
        }
        self.save_query_to_history();
        iced::exit()
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

    fn handle_file_browser_selected(&mut self, index: usize) -> Task<Message> {
        if let Some(entry) = self.file_browser.entries.get(index) {
            if entry.is_dir {
                let new_query = format!("{}/", entry.full_path);
                self.search_query = new_query.clone();
                return Task::done(Message::SearchChanged(new_query));
            }

            if self.view.current_modifiers.control() {
                spawn_copy(&entry.full_path);
            } else {
                let _ = Command::new("xdg-open")
                    .arg(&entry.full_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }

        self.save_query_to_history();
        iced::exit()
    }

    fn handle_file_browser_tab_complete(&mut self) -> Task<Message> {
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
            );
            let query = self.search_query.clone();
            return Task::done(Message::SearchChanged(query));
        }
        Task::none()
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
                let trimmed = self.search_query.trim();
                let emoji_query = extract_keyword_query(trimmed, emoji_trigger).unwrap_or_default();
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
        let mut offset = 0;
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
        self.filtered_configs.len() + offset
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
