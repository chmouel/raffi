use std::collections::HashMap;
use std::fs;
use std::path::Path;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, move_cursor_to_end, scroll_by, snap_to};
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::Task;

use super::actions::{
    copy_text_checked, execute_action, execute_file_action, execute_snippet_action,
    open_in_file_manager, DeferredAction,
};
use super::browser::read_directory;
use super::color::{color_inline_result, try_parse_color};
use super::command_list::{CommandListAddon, Selection};
use super::currency::{
    fetch_exchange_rate, fetch_multi_exchange_rates, is_currency_help_query,
    try_parse_currency_conversion, try_parse_multi_currency_conversion,
};
use super::devtools::{
    compute_hash_rows, generator_inline_result, transform_rows, try_parse_generator, DevRow,
    Generator, HASHES, HASH_DEBOUNCE, HASH_PENDING,
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
    CachedRate, ClipboardData, ClipboardState, CurrencyConversion, CurrencyConversionRequest,
    CurrencyResult, CurrencyState, DevtoolsState, EmojiEntry, EmojiState, FallbackAction,
    FileBrowserState, HistoryState, KeywordSuggestion, LauncherApp, Message, MultiCurrencyRequest,
    MultiCurrencyResult, PendingSelection, ResolvedFallback, ScriptFilterResult, ScriptFilterState,
    SharedSelection, TextSnippetState, ViewState, WebSearchActiveState, WebSearchState,
};
use super::support::{
    calculator_inline_result, fuzzy_match_configs, load_history, load_mru_map, mru_bounds,
    mru_sort_key, record_mru_use, save_history, save_mru_map, try_evaluate_math,
};
use super::system::system_entries;
use super::types::InlineProvider;
use super::units::{try_parse_unit_conversion, units_inline_result};
use super::world_clock::{current_time_inline_result, try_parse_time_query};
use crate::ui::{get_mru_cache_path, get_snippet_mru_cache_path, FontSizes};
use crate::{
    read_desktop_icon_map, read_icon_map, resolve_launcher_icon_path, AddonsConfig, RaffiConfig,
    SortMode,
};

#[derive(Clone, PartialEq, Eq)]
pub(super) enum QueryMode {
    FileBrowser {
        expanded: String,
        filter: String,
    },
    ScriptFilter {
        config_index: usize,
        query: String,
    },
    TextSnippet {
        config_index: usize,
        query: String,
    },
    Emoji {
        query: String,
    },
    Clipboard {
        query: String,
        images_only: bool,
    },
    WebSearch {
        config_index: usize,
        query: String,
    },
    CommandList {
        addon: CommandListAddon,
        query: String,
    },
    Devtools {
        payload: String,
    },
    Standard,
}

/// Debug output keeps the payload of sensitive modes out of logs.
impl std::fmt::Debug for QueryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileBrowser { expanded, filter } => f
                .debug_struct("FileBrowser")
                .field("expanded", expanded)
                .field("filter", filter)
                .finish(),
            Self::ScriptFilter {
                config_index,
                query,
            } => f
                .debug_struct("ScriptFilter")
                .field("config_index", config_index)
                .field("query", query)
                .finish(),
            Self::TextSnippet {
                config_index,
                query,
            } => f
                .debug_struct("TextSnippet")
                .field("config_index", config_index)
                .field("query", query)
                .finish(),
            Self::Emoji { query } => f.debug_struct("Emoji").field("query", query).finish(),
            Self::Clipboard { images_only, .. } => f
                .debug_struct("Clipboard")
                .field("images_only", images_only)
                .finish_non_exhaustive(),
            Self::WebSearch {
                config_index,
                query,
            } => f
                .debug_struct("WebSearch")
                .field("config_index", config_index)
                .field("query", query)
                .finish(),
            Self::CommandList { addon, .. } => f
                .debug_struct("CommandList")
                .field("addon", addon)
                .finish_non_exhaustive(),
            Self::Devtools { .. } => f.debug_struct("Devtools").finish_non_exhaustive(),
            Self::Standard => f.write_str("Standard"),
        }
    }
}

/// How a query is handled, worked out before anything filters or logs it.
/// `sensitive` queries stay out of search history and debug logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueryClass {
    pub mode: QueryMode,
    pub inline_provider: Option<InlineProvider>,
    pub sensitive: bool,
}

impl Default for QueryClass {
    fn default() -> Self {
        Self {
            mode: QueryMode::Standard,
            inline_provider: None,
            sensitive: false,
        }
    }
}

impl QueryClass {
    /// The query as it may appear in debug logs.
    pub(super) fn loggable_query(&self, query: &str, addons: &AddonsConfig) -> String {
        match &self.mode {
            QueryMode::Clipboard { images_only, .. } => {
                let clipboard = &addons.clipboard;
                let trigger = if *images_only {
                    clipboard.image_trigger()
                } else {
                    clipboard.trigger()
                };
                format!("{trigger} <redacted>")
            }
            _ if self.sensitive => "<redacted>".to_string(),
            _ => query.trim().to_string(),
        }
    }
}

pub(super) fn classify_query(query: &str, addons: &AddonsConfig) -> QueryClass {
    let mode = route_query(query, addons);
    let inline_provider = match mode {
        QueryMode::FileBrowser { .. }
        | QueryMode::Clipboard { .. }
        | QueryMode::CommandList { .. }
        | QueryMode::Devtools { .. } => None,
        _ => inline_provider(query, addons),
    };
    let sensitive = matches!(
        mode,
        QueryMode::Clipboard { .. } | QueryMode::CommandList { .. } | QueryMode::Devtools { .. }
    ) || matches!(
        inline_provider,
        Some(InlineProvider::Uuid | InlineProvider::Password)
    );
    QueryClass {
        mode,
        inline_provider,
        sensitive,
    }
}

/// The first inline parser matching `query`; it owns the inline result area.
fn inline_provider(query: &str, addons: &AddonsConfig) -> Option<InlineProvider> {
    if addons.calculator.enabled && try_evaluate_math(query).is_some() {
        return Some(InlineProvider::Calculator);
    }
    let currency = &addons.currency;
    if currency.enabled {
        let trigger = currency.trigger.as_deref().unwrap_or("$");
        let default_currency = currency.default_currency.as_deref().unwrap_or("USD");
        let currencies = currency.currencies.as_deref().unwrap_or_default();
        if try_parse_currency_conversion(query, default_currency, trigger).is_some()
            || try_parse_multi_currency_conversion(query, currencies, default_currency, trigger)
                .is_some()
        {
            return Some(InlineProvider::Currency);
        }
    }
    if addons.units.enabled && try_parse_unit_conversion(query).is_some() {
        return Some(InlineProvider::Units);
    }
    if addons.color.enabled && try_parse_color(query).is_some() {
        return Some(InlineProvider::Color);
    }
    if addons.world_clock.enabled && try_parse_time_query(query).is_some() {
        return Some(InlineProvider::Time);
    }
    if addons.devtools.enabled {
        match try_parse_generator(query, addons.devtools.password_length()) {
            Some(Generator::Uuid) => return Some(InlineProvider::Uuid),
            Some(Generator::Password(_)) => return Some(InlineProvider::Password),
            None => {}
        }
    }
    None
}

/// `in_flight` provider for inline result copies.
const INLINE_COPY: &str = "inline-copy";

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

/// Like `extract_keyword_query` but keeps the rest exactly as typed, since
/// whitespace changes hashes and encodings.
fn extract_raw_keyword_payload(input: &str, keyword: &str) -> Option<String> {
    input
        .strip_prefix(keyword)
        .and_then(|rest| rest.strip_prefix(' '))
        .map(str::to_string)
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

/// Theme icon names for the clipboard keywords; rows show no icon when the
/// theme lacks them.
pub(super) const CLIPBOARD_ICON: &str = "copyq";
pub(super) const CLIPBOARD_IMAGES_ICON: &str = "image-x-generic";
pub(super) const SYSTEM_ICON: &str = "system-shutdown";
pub(super) const WINDOWS_ICON: &str = "preferences-system-windows";
pub(super) const MEDIA_ICON: &str = "multimedia-player";
pub(super) const AUDIO_ICON: &str = "audio-speakers";
pub(super) const KILL_ICON: &str = "utilities-system-monitor";
pub(super) const BLUETOOTH_ICON: &str = "bluetooth-active";
pub(super) const DEVTOOLS_ICON: &str = "utilities-terminal";

/// The query as it may appear in debug logs: sensitive searches such as the
/// clipboard can contain copied secrets, so only their trigger is kept.
pub(super) fn loggable_query(query: &str, addons: &AddonsConfig) -> String {
    classify_query(query, addons).loggable_query(query, addons)
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

    if addons.clipboard.enabled {
        if let Some(query) = extract_keyword_query(input, addons.clipboard.trigger()) {
            return QueryMode::Clipboard {
                query,
                images_only: false,
            };
        }
        if let Some(query) = extract_keyword_query(input, addons.clipboard.image_trigger()) {
            return QueryMode::Clipboard {
                query,
                images_only: true,
            };
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

    if addons.system.enabled {
        if let Some(query) = extract_keyword_query(input, addons.system.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::System,
                query,
            };
        }
    }

    if addons.windows.enabled {
        if let Some(query) = extract_keyword_query(input, addons.windows.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Windows,
                query,
            };
        }
    }

    if addons.media.enabled {
        if let Some(query) = extract_keyword_query(input, addons.media.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Media,
                query,
            };
        }
    }

    if addons.audio.enabled {
        if let Some(query) = extract_keyword_query(input, addons.audio.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Audio,
                query,
            };
        }
    }

    if addons.kill.enabled {
        if let Some(query) = extract_keyword_query(input, addons.kill.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Kill,
                query,
            };
        }
    }

    if addons.bluetooth.enabled {
        if let Some(query) = extract_keyword_query(input, addons.bluetooth.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Bluetooth,
                query,
            };
        }
    }

    if addons.devtools.enabled {
        if let Some(payload) = extract_raw_keyword_payload(input, addons.devtools.trigger()) {
            return QueryMode::Devtools { payload };
        }
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
    let long_enough = input.chars().count() >= addons.keyword_min_chars();
    let matches = |keyword: &str| {
        let keyword = keyword.to_lowercase();
        keyword == input_lower || (long_enough && keyword.starts_with(&input_lower))
    };

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

    if addons.clipboard.enabled && matches(addons.clipboard.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.clipboard.trigger().to_string(),
            subtitle: "Browse clipboard history".to_string(),
            icon: Some(CLIPBOARD_ICON.to_string()),
        });
    }
    if addons.clipboard.enabled && matches(addons.clipboard.image_trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.clipboard.image_trigger().to_string(),
            subtitle: "Browse images in clipboard history".to_string(),
            icon: Some(CLIPBOARD_IMAGES_ICON.to_string()),
        });
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

    if addons.system.enabled && matches(addons.system.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.system.trigger().to_string(),
            subtitle: "Lock, suspend, log out, reboot or shut down".to_string(),
            icon: Some(SYSTEM_ICON.to_string()),
        });
    }

    if addons.windows.enabled && matches(addons.windows.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.windows.trigger().to_string(),
            subtitle: "Switch to an open window".to_string(),
            icon: Some(WINDOWS_ICON.to_string()),
        });
    }

    if addons.media.enabled && matches(addons.media.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.media.trigger().to_string(),
            subtitle: "Play, pause or skip in media players".to_string(),
            icon: Some(MEDIA_ICON.to_string()),
        });
    }

    if addons.audio.enabled && matches(addons.audio.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.audio.trigger().to_string(),
            subtitle: "Switch or mute audio outputs and inputs".to_string(),
            icon: Some(AUDIO_ICON.to_string()),
        });
    }

    if addons.kill.enabled && matches(addons.kill.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.kill.trigger().to_string(),
            subtitle: "Stop one of your processes".to_string(),
            icon: Some(KILL_ICON.to_string()),
        });
    }

    if addons.bluetooth.enabled && matches(addons.bluetooth.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.bluetooth.trigger().to_string(),
            subtitle: "Connect Bluetooth devices or turn Bluetooth on and off".to_string(),
            icon: Some(BLUETOOTH_ICON.to_string()),
        });
    }

    if addons.devtools.enabled && matches(addons.devtools.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.devtools.trigger().to_string(),
            subtitle: "Encode, decode, hash or reformat text".to_string(),
            icon: Some(DEVTOOLS_ICON.to_string()),
        });
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
            "LauncherApp::new: configs={} no_icons={} sort_mode={sort_mode:?} initial_query={:?}",
            configs.len(),
            no_icons,
            initial_query
                .as_deref()
                .map(|query| loggable_query(query, &addons))
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
                desktop_icon_map,
                no_icons,
                launcher_icon_paths,
                mru_map,
                sort_mode,
                addons,
                query_class: QueryClass::default(),
                inline_result: None,
                currency: CurrencyState::default(),
                script_filter: ScriptFilterState::default(),
                text_snippets,
                hyprland: None,
                deferred_action: Default::default(),
                in_flight: Default::default(),
                inline_generation: 0,
                inline_copy_error: None,
                #[cfg(test)]
                copier: None,
                program_exists: crate::find_binary,
                compositor: super::windows::Compositor::current,
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
                command_list: Default::default(),
                devtools: DevtoolsState::default(),
                clipboard: ClipboardState::default(),
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
        let task = self.update_inner(message);
        let preview = self.ensure_clipboard_preview();
        Task::batch([task, preview])
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => self.handle_search_changed(query),
            Message::MoveUp => self.move_selection(-1),
            Message::MoveDown => self.move_selection(1),
            Message::Submit => self.activate_index(self.selected_index),
            Message::SubmitRepeat if self.command_list.is_active() => Task::none(),
            Message::SubmitRepeat => self.activate_index(self.selected_index),
            Message::Cancel => {
                if self.view.help_active {
                    return self.close_help();
                }
                iced::exit()
            }
            Message::ItemClicked(index) => {
                if index != self.selected_index {
                    self.command_list.armed = None;
                }
                self.selected_index = index;
                self.activate_index(index)
            }
            Message::InlineSelected(index) => self.handle_inline_selected(index),
            Message::InlineCopied(generation, result) => {
                self.handle_inline_copied(generation, result)
            }
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
                if modifiers != self.view.current_modifiers {
                    self.command_list.armed = None;
                }
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
            Message::CommandListSelected(index) => self.handle_command_list_selected(index),
            Message::CommandListLoaded(generation, result) => {
                self.handle_command_list_loaded(generation, result)
            }
            Message::CommandListActionDone(addon, session, identity, result) => {
                self.handle_command_list_action_done(addon, session, identity, result)
            }
            Message::DevtoolsSelected(index) => self.handle_devtools_selected(index),
            Message::DevtoolsHashes(generation, rows) => {
                self.handle_devtools_hashes(generation, rows)
            }
            Message::FallbackSelected(index) => self.handle_fallback_selected(index),
            Message::KeywordSuggestionSelected(index) => {
                self.handle_keyword_suggestion_selected(index)
            }
            Message::ClipboardPage(generation, result) => {
                self.handle_clipboard_page(generation, result)
            }
            Message::ClipboardPreview(session, id, result) => {
                self.handle_clipboard_preview(session, id, result)
            }
            Message::ClipboardSelected(index) => self.handle_clipboard_selected(index),
            Message::ClipboardActionDone(session, result) => {
                self.handle_clipboard_action_done(session, result)
            }
            Message::ClipboardRefresh => self.handle_clipboard_refresh(),
            Message::ClipboardToggleImages => self.handle_clipboard_toggle_images(),
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

        let class = classify_query(&query, &self.addons);
        let log_query = class.loggable_query(&query, &self.addons);
        self.search_query = query.clone();
        self.invalidate_inline_copy();
        self.filter_items(&query, &log_query);
        self.inline_result = match class.inline_provider {
            Some(InlineProvider::Calculator) => {
                try_evaluate_math(&query).map(|calc| calculator_inline_result(&calc))
            }
            Some(InlineProvider::Units) => {
                try_parse_unit_conversion(&query).map(|conversion| units_inline_result(&conversion))
            }
            Some(InlineProvider::Color) => {
                try_parse_color(&query).map(|color| color_inline_result(&color))
            }
            Some(InlineProvider::Time) => {
                try_parse_time_query(&query).map(|time| current_time_inline_result(&time))
            }
            Some(InlineProvider::Uuid | InlineProvider::Password) => {
                try_parse_generator(&query, self.addons.devtools.password_length())
                    .and_then(generator_inline_result)
            }
            _ => None,
        };
        self.selected_index = 0;

        if let Some(index) = self.history.index {
            if self.history.items.get(index).map(|h| h.as_str()) != Some(query.as_str()) {
                self.history.index = None;
            }
        }

        self.view.refresh_ids();

        let mut tasks = Vec::new();

        crate::debug_log!("route_query: query={log_query:?} -> {:?}", class.mode);
        let qmode = class.mode.clone();
        self.query_class = class;
        if !matches!(qmode, QueryMode::Clipboard { .. }) {
            self.clipboard.clear();
        }
        // Staying in the same command list keeps its loaded items.
        if !matches!(&qmode, QueryMode::CommandList { addon, .. }
            if self.command_list.addon == Some(*addon))
        {
            self.command_list.clear();
        }
        self.devtools.clear();
        self.keyword_suggestions = if matches!(qmode, QueryMode::Standard) {
            keyword_suggestions(&query, &self.addons)
        } else {
            Vec::new()
        };
        match qmode {
            QueryMode::FileBrowser { expanded, filter } => {
                self.handle_file_browser_query(&expanded, &filter);
                self.filtered_configs.clear();
                self.inline_result = None;
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
            QueryMode::Clipboard { query, images_only } => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.text_snippets.clear();
                self.emoji.clear();
                self.web_search.clear();
                self.currency.clear();
                self.filtered_configs.clear();
                self.inline_result = None;
                if let Some(drag) = &self.drag {
                    drag.disarm();
                }
                self.handle_clipboard_query(query, images_only, &mut tasks);
                return Task::batch(tasks);
            }
            QueryMode::CommandList { addon, query } => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.text_snippets.clear();
                self.emoji.clear();
                self.web_search.clear();
                self.currency.clear();
                self.filtered_configs.clear();
                self.inline_result = None;
                if let Some(drag) = &self.drag {
                    drag.disarm();
                }
                tasks.push(self.handle_command_list_query(addon, query));
                return Task::batch(tasks);
            }
            QueryMode::Devtools { payload } => {
                self.file_browser.clear();
                self.script_filter.clear();
                self.text_snippets.clear();
                self.emoji.clear();
                self.web_search.clear();
                self.currency.clear();
                self.filtered_configs.clear();
                self.inline_result = None;
                if let Some(drag) = &self.drag {
                    drag.disarm();
                }
                tasks.push(self.handle_devtools_query(payload));
                return Task::batch(tasks);
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

        if !self.addons.currency.enabled
            || self.query_class.inline_provider != Some(InlineProvider::Currency)
        {
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

    /// Show `addon`'s list filtered by `query`. System entries are rebuilt
    /// for every query; windows load once when the mode is entered.
    fn handle_command_list_query(
        &mut self,
        addon: CommandListAddon,
        query: String,
    ) -> Task<Message> {
        let entered = self.command_list.enter(addon);
        self.command_list.set_query(query);
        match addon {
            CommandListAddon::System => {
                let session_id = std::env::var("XDG_SESSION_ID").ok();
                let items = system_entries(
                    &self.addons.system,
                    (self.compositor)(),
                    session_id.as_deref().filter(|id| !id.is_empty()),
                    self.program_exists,
                )
                .iter()
                .map(|entry| entry.to_item())
                .collect();
                self.set_command_list_items(items);
                Task::none()
            }
            CommandListAddon::Windows if !entered => Task::none(),
            CommandListAddon::Windows => {
                let Some(compositor) = (self.compositor)() else {
                    self.command_list
                        .set_notice("Window switching needs Hyprland or niri".to_string());
                    return Task::none();
                };
                if !(self.program_exists)(compositor.program()) {
                    self.command_list
                        .set_notice(format!("{} not found", compositor.program()));
                    return Task::none();
                }
                self.load_command_list(move || compositor.load_windows(), false)
            }
            CommandListAddon::Media if !entered => Task::none(),
            CommandListAddon::Media => {
                if !(self.program_exists)("playerctl") {
                    self.command_list
                        .set_notice("playerctl not found".to_string());
                    return Task::none();
                }
                self.load_media(false)
            }
            CommandListAddon::Audio if !entered => Task::none(),
            CommandListAddon::Audio => {
                if !(self.program_exists)("pactl") {
                    self.command_list.set_notice("pactl not found".to_string());
                    return Task::none();
                }
                self.load_audio(false)
            }
            CommandListAddon::Kill if !entered => Task::none(),
            CommandListAddon::Kill => {
                if !(self.program_exists)("kill") {
                    self.command_list.set_notice("kill not found".to_string());
                    return Task::none();
                }
                self.load_command_list(super::kill::load_processes, false)
            }
            CommandListAddon::Bluetooth if !entered => Task::none(),
            CommandListAddon::Bluetooth => {
                if !(self.program_exists)("bluetoothctl") {
                    self.command_list
                        .set_notice("bluetoothctl not found".to_string());
                    return Task::none();
                }
                self.load_bluetooth(false)
            }
        }
    }

    fn load_bluetooth(&mut self, refresh: bool) -> Task<Message> {
        let keep_open = !self.addons.bluetooth.close_on_action;
        self.load_command_list(
            move || Ok(super::bluetooth::load_devices(keep_open)),
            refresh,
        )
    }

    fn load_audio(&mut self, refresh: bool) -> Task<Message> {
        let keep_open = !self.addons.audio.close_on_action;
        self.load_command_list(move || Ok(super::audio::load_devices(keep_open)), refresh)
    }

    fn load_media(&mut self, refresh: bool) -> Task<Message> {
        let keep_open = !self.addons.media.close_on_action;
        self.load_command_list(move || super::media::load_players(keep_open), refresh)
    }

    /// Run `load` off the UI thread; its result is dropped if the list has
    /// moved on by then. A refresh keeps the current rows until it lands.
    fn load_command_list(
        &mut self,
        load: impl FnOnce() -> Result<Vec<super::command_list::CommandItem>, String> + Send + 'static,
        refresh: bool,
    ) -> Task<Message> {
        let generation = if refresh {
            self.command_list.begin_refresh()
        } else {
            self.command_list.begin_load()
        };
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(load());
                });
                receiver
                    .await
                    .unwrap_or_else(|_| Err("loading stopped unexpectedly".to_string()))
            },
            move |result| Message::CommandListLoaded(generation, result),
        )
    }

    fn handle_command_list_loaded(
        &mut self,
        generation: u64,
        result: Result<Vec<super::command_list::CommandItem>, String>,
    ) -> Task<Message> {
        if !self.command_list.is_current(generation) {
            return Task::none();
        }
        let selected = self
            .command_list
            .item(self.selected_index)
            .map(|item| item.identity.clone());
        match result {
            Ok(items) => self.set_command_list_items(items),
            Err(error) => {
                crate::debug_log!("command list: load failed: {error}");
                self.command_list.set_notice(error);
            }
        }
        self.selected_index = selected
            .and_then(|identity| self.command_list.position(&identity))
            .unwrap_or(0);
        Task::none()
    }

    /// Release the action's guard, then refresh its list, or show the error
    /// above it, unless the user has moved on to another list.
    fn handle_command_list_action_done(
        &mut self,
        addon: CommandListAddon,
        session: u64,
        identity: String,
        result: Result<(), String>,
    ) -> Task<Message> {
        self.in_flight.finish(addon.name(), &identity);
        if self.command_list.addon != Some(addon) || self.command_list.session != session {
            return Task::none();
        }
        match result {
            Ok(()) => match addon {
                CommandListAddon::Media => self.load_media(true),
                CommandListAddon::Audio => self.load_audio(true),
                CommandListAddon::Kill => self.load_command_list(super::kill::load_processes, true),
                CommandListAddon::Bluetooth => self.load_bluetooth(true),
                CommandListAddon::System | CommandListAddon::Windows => Task::none(),
            },
            Err(error) => {
                crate::debug_log!("{}: action failed: {error}", addon.name());
                let error = match self
                    .command_list
                    .items
                    .iter()
                    .find(|item| item.identity == identity)
                {
                    Some(item) => format!("{} failed: {error}", item.title),
                    None => error,
                };
                let selected = self
                    .command_list
                    .item(self.selected_index)
                    .map(|item| item.identity.clone());
                // A failed signal usually means the process is gone or was
                // replaced, and a timed out connection may still have
                // changed the device, so the list reloads under the error
                // row.
                let refresh = match addon {
                    CommandListAddon::Kill => {
                        self.load_command_list(super::kill::load_processes, true)
                    }
                    CommandListAddon::Bluetooth => self.load_bluetooth(true),
                    _ => Task::none(),
                };
                self.set_command_list_front(super::command_list::CommandItem::info(
                    "error",
                    error,
                    "dialog-error",
                ));
                if let Some(row) = selected.and_then(|id| self.command_list.position(&id)) {
                    self.selected_index = row;
                }
                refresh
            }
        }
    }

    fn set_command_list_front(&mut self, mut item: super::command_list::CommandItem) {
        item.icon = item
            .icon_names
            .iter()
            .find_map(|name| self.resolve_icon_name(name));
        self.command_list.show_error(item);
    }

    /// What `item` shows while its action runs with Raffi kept open.
    pub(super) fn command_list_progress<'a>(
        &self,
        item: &'a super::command_list::CommandItem,
    ) -> Option<&'a str> {
        let addon = self.command_list.addon?;
        if !self.in_flight.is_pending(addon.name(), &item.identity) {
            return None;
        }
        item.primary.as_ref()?.progress.as_deref()
    }

    fn set_command_list_items(&mut self, mut items: Vec<super::command_list::CommandItem>) {
        for item in &mut items {
            item.icon = item
                .icon_names
                .iter()
                .find_map(|name| self.resolve_icon_name(name));
        }
        self.command_list.set_items(items);
    }

    /// A theme icon path for `name`, directly or through the desktop entry
    /// of the executable with that name.
    fn resolve_icon_name(&self, name: &str) -> Option<String> {
        self.icon_map.get(name).cloned().or_else(|| {
            self.desktop_icon_map
                .get(name)
                .and_then(|icon| crate::resolve_icon_reference(icon, &self.icon_map))
        })
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

        self.command_list.armed = None;
        self.follow_clipboard_selection();
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

        if self.clipboard.active {
            let item_count = self.clipboard.row_count();
            if index >= current_index && index < current_index + item_count {
                return self.update(Message::ClipboardSelected(index - current_index));
            }
            current_index += item_count;
        }

        if self.command_list.is_active() {
            let item_count = self.command_list.row_count();
            if index >= current_index && index < current_index + item_count {
                return self.update(Message::CommandListSelected(index - current_index));
            }
            current_index += item_count;
        }

        if self.devtools.active {
            let item_count = self.devtools.rows.len();
            if index >= current_index && index < current_index + item_count {
                return self.update(Message::DevtoolsSelected(index - current_index));
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

        if let Some(inline) = &self.inline_result {
            let item_count = inline.rows.len();
            if index >= current_index && index < current_index + item_count {
                return self.update(Message::InlineSelected(index - current_index));
            }
            current_index += item_count;
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

    fn handle_inline_selected(&mut self, index: usize) -> Task<Message> {
        let value = self.inline_copy_value(index);
        self.start_checked_copy(value)
    }

    /// Value copied for inline row `index`, Ctrl picking `alt_copy_value`.
    pub(super) fn inline_copy_value(&self, index: usize) -> Option<String> {
        let inline = self.inline_result.as_ref()?;
        let row = inline.rows.get(index)?;
        if row.copy_value.is_empty() {
            return None;
        }
        crate::debug_log!("inline: copying {:?} row {index}", inline.provider);
        let value = match &row.alt_copy_value {
            Some(alt) if self.view.current_modifiers.control() => alt,
            _ => &row.copy_value,
        };
        Some(value.clone())
    }

    /// Copies `value` off the UI thread; `Message::InlineCopied` closes
    /// Raffi on success or shows an error row on failure.
    fn start_checked_copy(&mut self, value: Option<String>) -> Task<Message> {
        if self.in_flight.is_pending(INLINE_COPY, "") {
            return Task::none();
        }
        let Some(value) = value else {
            return Task::none();
        };
        let copy = self.copy_job(value);
        self.in_flight.begin(INLINE_COPY, "");
        self.inline_copy_error = None;
        let generation = self.inline_generation;
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(copy());
                });
                receiver
                    .await
                    .unwrap_or_else(|_| Err("copy worker stopped unexpectedly".into()))
            },
            move |result| Message::InlineCopied(generation, result),
        )
    }

    /// The blocking checked copy of `value`.
    pub(super) fn copy_job(&self, value: String) -> Box<dyn FnOnce() -> Result<(), String> + Send> {
        #[cfg(test)]
        if let Some(argv) = self.copier.clone() {
            return Box::new(move || super::actions::copy_text_with(&argv, &value));
        }
        Box::new(move || copy_text_checked(&value))
    }

    /// A query or mode change makes any running inline copy stale.
    fn invalidate_inline_copy(&mut self) {
        self.inline_generation += 1;
        self.inline_copy_error = None;
    }

    /// A stale completion only releases the guard: it neither closes Raffi
    /// nor touches the state of the current query.
    fn handle_inline_copied(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        self.in_flight.finish(INLINE_COPY, "");
        if generation != self.inline_generation {
            return Task::none();
        }
        match result {
            Ok(()) => {
                self.save_query_to_history();
                iced::exit()
            }
            Err(error) => {
                self.inline_copy_error = Some(format!("Copy failed: {error}"));
                Task::none()
            }
        }
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
        let value = self
            .currency
            .result
            .as_ref()
            .map(|currency| format!("{:.2}", currency.converted_amount));
        self.start_checked_copy(value)
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
        let value = self
            .currency
            .multi_result
            .as_ref()
            .and_then(|result| result.conversions.get(index))
            .map(|conversion| format!("{:.2}", conversion.converted_amount));
        self.start_checked_copy(value)
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
            self.deferred_action
                .lock()
                .map_err(|_| "cannot queue Hyprland paste".to_string())?
                .replace(DeferredAction::HyprPaste(target));
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

    /// Runs the item's action after Raffi closes. Actions needing
    /// confirmation run on a second Enter on the same target with the same
    /// modifiers.
    fn handle_command_list_selected(&mut self, row: usize) -> Task<Message> {
        let Some(addon) = self.command_list.addon else {
            return Task::none();
        };
        match self.command_list.select(row, self.view.current_modifiers) {
            Selection::Ignore => Task::none(),
            Selection::Armed => {
                crate::debug_log!("{}: arming confirmation", addon.name());
                Task::none()
            }
            Selection::Run(action, identity, keep_open) => {
                if !self.in_flight.begin(addon.name(), &identity) {
                    return Task::none();
                }
                if keep_open {
                    crate::debug_log!("{}: running {identity}", addon.name());
                    let session = self.command_list.session;
                    return Task::perform(
                        async move {
                            let (sender, receiver) = iced::futures::channel::oneshot::channel();
                            std::thread::spawn(move || {
                                let _ = sender.send(super::command_list::run_kept_open(&action));
                            });
                            let result = receiver
                                .await
                                .unwrap_or_else(|_| Err("action stopped unexpectedly".to_string()));
                            (identity, result)
                        },
                        move |(identity, result)| {
                            Message::CommandListActionDone(addon, session, identity, result)
                        },
                    );
                }
                crate::debug_log!("{}: running {identity} after close", addon.name());
                if let Ok(mut slot) = self.deferred_action.lock() {
                    *slot = Some(action);
                }
                iced::exit()
            }
        }
    }

    /// Builds the transform rows at once; hashes follow after a short
    /// debounce, off the UI thread.
    fn handle_devtools_query(&mut self, payload: String) -> Task<Message> {
        self.devtools.active = true;
        self.devtools.rows = transform_rows(&payload);
        if payload.is_empty() {
            return Task::none();
        }
        let jobs = self.devtools_hash_jobs();
        for (title, argv) in &jobs {
            self.devtools.rows.push(match argv {
                Ok(_) => DevRow::status(title, HASH_PENDING),
                Err(missing) => DevRow::status(title, missing.as_str()),
            });
        }
        let jobs: Vec<_> = jobs
            .into_iter()
            .filter_map(|(title, argv)| argv.ok().map(|argv| (title, argv)))
            .collect();
        if jobs.is_empty() {
            return Task::none();
        }
        let generation = self.devtools.current_generation();
        let current = self.devtools.hash_generation.clone();
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    std::thread::sleep(HASH_DEBOUNCE);
                    let rows = if current.load(std::sync::atomic::Ordering::SeqCst) == generation {
                        compute_hash_rows(&jobs, &payload)
                    } else {
                        Vec::new()
                    };
                    let _ = sender.send(rows);
                });
                receiver.await.unwrap_or_default()
            },
            move |rows| Message::DevtoolsHashes(generation, rows),
        )
    }

    /// Each hash row with the command computing it, or the status shown
    /// when its tool is not installed.
    pub(super) fn devtools_hash_jobs(&self) -> Vec<(&'static str, Result<Vec<String>, String>)> {
        HASHES
            .iter()
            .map(|&(title, program)| {
                if !(self.program_exists)(program) {
                    return (title, Err(format!("{program} not found")));
                }
                #[cfg(test)]
                if let Some(argv) = &self.devtools.hash_command {
                    return (title, Ok(argv.clone()));
                }
                (title, Ok(vec![program.to_string()]))
            })
            .collect()
    }

    fn handle_devtools_hashes(&mut self, generation: u64, rows: Vec<DevRow>) -> Task<Message> {
        if !self.devtools.active || generation != self.devtools.current_generation() {
            return Task::none();
        }
        for row in rows {
            if let Some(slot) = self
                .devtools
                .rows
                .iter_mut()
                .find(|slot| slot.title == row.title)
            {
                *slot = row;
            }
        }
        Task::none()
    }

    fn handle_devtools_selected(&mut self, index: usize) -> Task<Message> {
        let value = self
            .devtools
            .rows
            .get(index)
            .filter(|row| row.copyable)
            .map(|row| row.value.clone());
        self.start_checked_copy(value)
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

    pub(super) fn filter_items(&mut self, query: &str, log_query: &str) {
        if query.is_empty() {
            self.filtered_configs = (0..self.configs.len()).collect();
        } else {
            self.filtered_configs = fuzzy_match_configs(&self.configs, query, log_query);
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
        offset += self.clipboard.row_count();
        offset += self.command_list.row_count();
        if self.devtools.active {
            offset += self.devtools.rows.len();
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
        if let Some(inline) = &self.inline_result {
            offset += inline.rows.len();
        }
        self.filtered_configs.len() + offset + self.fallback_count()
    }

    fn is_in_addon_mode(&self) -> bool {
        self.script_filter.results.is_some()
            || self.script_filter.loading
            || self.text_snippets.active
            || self.text_snippets.loading
            || self.emoji.active
            || self.clipboard.active
            || self.command_list.is_active()
            || self.devtools.active
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
        self.inline_result = None;
        self.invalidate_inline_copy();
        self.script_filter.clear();
        self.text_snippets.clear();
        self.emoji.clear();
        self.clipboard.clear();
        self.command_list.clear();
        self.devtools.clear();
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
        if self.query_class.sensitive {
            return;
        }
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
