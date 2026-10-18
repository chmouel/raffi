use std::collections::HashMap;
use std::path::Path;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, move_cursor_to_end, scroll_by, snap_to};
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::Task;

use super::actions::{
    copy_image_checked, copy_text_checked, execute_action, execute_action_with_env,
    execute_file_action, execute_snippet_action, open_in_file_manager, DeferredAction,
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
use super::help::{add_main_view_shortcut, filter_help_entries, help_entries};
use super::main_view::{Block, Focus, Source, MAIN_VIEW_SESSION};
use super::pass::PassContext;
use super::preview::image_mime_type;
use super::recent::{RecentPick, StoredItem};
use super::script_filters::{
    execute_script_filter, execute_script_filter_preview, parse_script_filter_output,
    schedule_script_filter_preview, PreviewRequest,
};
use super::secret_io::{SecretError, SystemRunner};
use super::snippets::{
    compute_previews, execute_text_snippet_command, expand_final, fetch_clipboard, filter_snippets,
    missing_clipboard, read_snippet_dir, read_snippet_file, snippet_mru_key,
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
use super::vault::{
    Backend, EntryAction, Field, JobSettings, Op, OpKind, Outcome, PromptState, Report, Request,
    VaultAddon,
};
use super::world_clock::{current_time_inline_result, try_parse_time_query};
use crate::secret::Secret;
use crate::ui::{
    get_mru_cache_path, get_snippet_mru_cache_path, get_vault_mru_cache_path, FontSizes,
};
use crate::{
    read_desktop_icon_map, read_icon_map, resolve_launcher_icon_path, AddonsConfig, RaffiConfig,
    SortMode,
};
use std::time::Duration;

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

fn checked_clipboard_copy_task(
    path: String,
    copy: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Task<Message> {
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
        move |result| Message::FileBrowserCopyCompleted(path, result),
    )
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
/// `sensitive` queries stay out of debug logs, and out of search history
/// unless [`QueryClass::saved_in_history`] lets them in.
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
    /// pass and Bitwarden queries are saved: they only filter entry names,
    /// and secrets never reach the query.
    pub(super) fn saved_in_history(&self) -> bool {
        match &self.mode {
            QueryMode::CommandList { addon, .. } if VaultAddon::of(*addon).is_some() => true,
            QueryMode::CommandList {
                addon: CommandListAddon::Terminal,
                ..
            } => true,
            _ => !self.sensitive,
        }
    }

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
/// Wait before running a Script Filter dynamic preview, so moving quickly
/// through results does not start one process per row.
const SCRIPT_FILTER_PREVIEW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(80);

/// Look up the page preview of `tab` for `list`, unless one is running or
/// already known.
pub(super) fn request_tab_preview(
    list: &mut super::command_list::CommandListState,
    tab: &super::browser_tabs::TabAction,
    service: crate::FaviconService,
) -> Task<Message> {
    list.preview_wanted.set(&tab.url);
    if list.preview_inflight.is_some() || list.previews.contains_key(&tab.url) {
        return Task::none();
    }
    let url = tab.url.clone();
    let browser = crate::browser_host::Browser {
        id: tab.browser_id.clone(),
        name: String::new(),
        pid: tab.browser_pid,
    };
    let session = list.session;
    let wanted = list.preview_wanted.clone();
    list.preview_inflight = Some(url.clone());
    Task::perform(
        {
            let url = url.clone();
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(super::tab_preview::load(&url, &browser, service, &wanted));
                });
                receiver.await.ok().flatten()
            }
        },
        move |bodies| Message::TabPreviewLoaded(session, url.clone(), bodies),
    )
}

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

/// Whether `new` is `old` with exactly one `c` inserted somewhere.
pub(super) fn is_single_insert(old: &str, new: &str, c: char) -> bool {
    if new.len() != old.len() + c.len_utf8() {
        return false;
    }
    let prefix = old
        .char_indices()
        .zip(new.chars())
        .find(|((_, a), b)| a != b)
        .map_or(old.len(), |((i, _), _)| i);
    new[prefix..].starts_with(c) && new[prefix + c.len_utf8()..] == old[prefix..]
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

/// `path` as it should appear in the input: under `home`, it starts with `~`.
pub(super) fn tilde_path(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

/// Emacs-style restart: typing `/` after a `/` or a `/~` drops everything
/// before it, so `~/Documents/~/` becomes `~/` and `~/Documents//` becomes `/`.
pub(super) fn fido_restart(old: &str, new: &str) -> Option<String> {
    if new.strip_suffix('/') != Some(old) {
        return None;
    }
    if old.ends_with("/~") {
        Some("~/".to_string())
    } else if old.ends_with('/') {
        Some("/".to_string())
    } else {
        None
    }
}

/// Theme icon names for the clipboard keywords; rows show no icon when the
/// theme lacks them.
pub(super) const CLIPBOARD_ICON: &str = "copyq";
pub(super) const CLIPBOARD_IMAGES_ICON: &str = "image-x-generic";
pub(super) const SYSTEM_ICON: &str = "system-shutdown";
pub(super) const WINDOWS_ICON: &str = "preferences-system-windows";
pub(super) const BROWSER_TABS_ICON: &str = "web-browser";
pub(super) const MEDIA_ICON: &str = "multimedia-player";
pub(super) const AUDIO_ICON: &str = "audio-speakers";
pub(super) const KILL_ICON: &str = "utilities-system-monitor";
pub(super) const BLUETOOTH_ICON: &str = "bluetooth-active";
pub(super) const PASS_ICON: &str = "dialog-password";
pub(super) const BITWARDEN_ICON: &str = "bitwarden";
pub(super) const DEVTOOLS_ICON: &str = "utilities-terminal";

/// Whether `message` comes from the user acting on the list or the query,
/// which the password prompt must not let through.
fn is_user_input(message: &Message) -> bool {
    matches!(
        message,
        Message::SearchChanged(_)
            | Message::MoveUp
            | Message::MoveDown
            | Message::SubmitRepeat
            | Message::ItemClicked(_)
            | Message::InlineSelected(_)
            | Message::CurrencyResultCopied
            | Message::MultiCurrencyResultCopied(_)
            | Message::ScriptFilterItemSelected(_)
            | Message::TextSnippetSelected(_)
            | Message::WebSearchSelected
            | Message::FileBrowserItemSelected(_)
            | Message::FileBrowserRowPressed { .. }
            | Message::FileBrowserRowReleased(_)
            | Message::TabComplete
            | Message::MainViewCycle(_)
            | Message::FileBrowserToggleHidden
            | Message::FileBrowserCopyPath
            | Message::FileBrowserCopyImage
            | Message::FileBrowserOpenSelected
            | Message::FileBrowserOpenFileManager
            | Message::FileBrowserCycleSort
            | Message::FileBrowserCycleShow
            | Message::HistoryPrevious
            | Message::HistoryNext
            | Message::ToggleHelp
            | Message::TogglePreview
            | Message::PreviewScrollDown
            | Message::PreviewScrollUp
            | Message::HelpEntrySelected(_)
            | Message::EmojiSelected(_)
            | Message::CommandListSelected(_)
            | Message::DevtoolsSelected(_)
            | Message::FallbackSelected(_)
            | Message::KeywordSuggestionSelected(_)
            | Message::ClipboardSelected(_)
            | Message::ClipboardRefresh
            | Message::ClipboardToggleImages
            | Message::ClipboardDelete
            | Message::VaultShortcut(_)
    )
}

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

    if addons.browser_tabs.enabled {
        if let Some(query) = extract_keyword_query(input, addons.browser_tabs.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::BrowserTabs,
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

    if addons.pass.enabled {
        if let Some(query) = extract_keyword_query(input, addons.pass.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Pass,
                query,
            };
        }
    }

    if addons.bitwarden.enabled {
        if let Some(query) = extract_keyword_query(input, addons.bitwarden.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Bitwarden,
                query,
            };
        }
    }

    if addons.devtools.enabled {
        if let Some(payload) = extract_raw_keyword_payload(input, addons.devtools.trigger()) {
            return QueryMode::Devtools { payload };
        }
    }

    if addons.terminal.enabled {
        if let Some(query) = extract_keyword_query(input, addons.terminal.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Terminal,
                query,
            };
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
            subtitle: "Lock, suspend, hibernate, log out, reboot or shut down".to_string(),
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

    if addons.browser_tabs.enabled && matches(addons.browser_tabs.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.browser_tabs.trigger().to_string(),
            subtitle: "Switch to or close a browser tab".to_string(),
            icon: Some(BROWSER_TABS_ICON.to_string()),
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

    if addons.pass.enabled && matches(addons.pass.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.pass.trigger().to_string(),
            subtitle: "Copy or type passwords from pass".to_string(),
            icon: Some(PASS_ICON.to_string()),
        });
    }

    if addons.bitwarden.enabled && matches(addons.bitwarden.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.bitwarden.trigger().to_string(),
            subtitle: "Copy or type passwords from Bitwarden".to_string(),
            icon: Some(BITWARDEN_ICON.to_string()),
        });
    }

    if addons.devtools.enabled && matches(addons.devtools.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.devtools.trigger().to_string(),
            subtitle: "Encode, decode, hash or reformat text".to_string(),
            icon: Some(DEVTOOLS_ICON.to_string()),
        });
    }

    if addons.terminal.enabled && matches(addons.terminal.trigger()) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.terminal.trigger().to_string(),
            subtitle: "Run a command in a terminal".to_string(),
            icon: Some(super::terminal::TERMINAL_ICON.to_string()),
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
        let file_browser_show = addons.file_browser.show;

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
                select_1_query: None,
                select_1_ready: false,
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
                terminal_tab: crate::terminal::find_tab_target,
                web_search: WebSearchState::default(),
                file_browser: FileBrowserState {
                    show_hidden: file_browser_show_hidden,
                    sort: file_browser_sort,
                    sort_reverse: file_browser_sort_reverse,
                    show: file_browser_show,
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
                vault: {
                    let mut vault = super::vault::VaultState::default();
                    vault.usage_path = get_vault_mru_cache_path().ok();
                    vault.usage = load_mru_map(vault.usage_path.as_deref());
                    vault
                },
                devtools: DevtoolsState::default(),
                clipboard: ClipboardState::default(),
                fallbacks,
                main_view: Default::default(),
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
        self.sync_vault();
        let select_1 = self.maybe_auto_select_1();
        let preview = self.ensure_clipboard_preview();
        let file_preview = self.ensure_file_browser_preview();
        let tab_preview = self.ensure_tab_preview();
        Task::batch([task, select_1, preview, file_preview, tab_preview])
    }

    fn maybe_auto_select_1(&mut self) -> Task<Message> {
        if !self.select_1_ready {
            return Task::none();
        }
        let Some(query) = self
            .select_1_query
            .as_deref()
            .filter(|query| !query.is_empty())
        else {
            self.select_1_query = None;
            self.select_1_ready = false;
            return Task::none();
        };
        if self.search_query != query {
            self.select_1_query = None;
            self.select_1_ready = false;
            return Task::none();
        }
        if self.select_1_results_loading() {
            return Task::none();
        }
        let index = self.sole_selectable_result_index();
        self.select_1_query = None;
        self.select_1_ready = false;
        if let Some(index) = index {
            self.activate_index(index)
        } else {
            Task::none()
        }
    }

    fn select_1_results_loading(&self) -> bool {
        match &self.query_class.mode {
            QueryMode::ScriptFilter { .. } => self.script_filter.loading,
            QueryMode::TextSnippet { .. } => self.text_snippets.loading,
            QueryMode::Emoji { .. } => self.emoji.data_loading,
            QueryMode::Clipboard { .. } => self.clipboard.loading,
            QueryMode::CommandList { addon, .. } => {
                self.command_list.loading
                    || self.command_list.refreshing
                    || (VaultAddon::of(*addon).is_some()
                        && (self.vault.busy()
                            || self.vault.prompt.is_some()
                            || self.vault.search_query.is_some()))
            }
            QueryMode::Devtools { .. } => self.devtools.loading,
            QueryMode::Standard => {
                self.currency.loading || self.currency.multi_loading || self.main_view.has_loading()
            }
            QueryMode::FileBrowser { .. } | QueryMode::WebSearch { .. } => false,
        }
    }

    fn sole_selectable_result_index(&self) -> Option<usize> {
        if self.view.help_active {
            return None;
        }
        let (selectable_count, candidate) = {
            let mut candidate = None;
            let mut selectable_count = 0;
            let mut index = 0;
            let mut consider = |selectable: bool| {
                if selectable {
                    selectable_count += 1;
                    if selectable_count == 1 {
                        candidate = Some(index);
                    }
                }
                index += 1;
            };

            for _ in &self.keyword_suggestions {
                consider(true);
            }
            if self.script_filter.loading {
                consider(false);
            } else if let Some(result) = &self.script_filter.results {
                for _ in &result.items {
                    consider(true);
                }
            }
            if self.text_snippets.loading {
                consider(false);
            } else if self.text_snippets.active {
                for _ in &self.text_snippets.filtered {
                    consider(true);
                }
            }
            if self.emoji.active {
                for _ in &self.emoji.filtered {
                    consider(true);
                }
            }
            for row in 0..self.clipboard.row_count() {
                consider(self.clipboard.items.get(row).is_some());
            }
            for row in 0..self.command_list.row_count() {
                consider(
                    self.selectable_command_item(
                        self.command_list.addon,
                        self.command_list.item(row),
                    ),
                );
            }
            if self.devtools.active {
                for row in &self.devtools.rows {
                    consider(row.copyable);
                }
            }
            if self.file_browser.active {
                for _ in &self.file_browser.entries {
                    consider(true);
                }
            }
            if self.web_search.active.is_some() {
                consider(true);
            }
            if self.currency.help {
                consider(false);
            }
            if self.currency.loading {
                consider(false);
            } else if self.currency.result.is_some() {
                consider(true);
            }
            if self.currency.multi_loading {
                consider(false);
            } else if let Some(result) = &self.currency.multi_result {
                for _ in &result.conversions {
                    consider(true);
                }
            }
            if let Some(inline) = &self.inline_result {
                for row in &inline.rows {
                    consider(!row.copy_value.is_empty());
                }
            }
            for (source, row) in self.main_view.rows(Block::BeforeLaunchers) {
                let selectable = match &self.main_view.sources[source] {
                    Source::List(list) => self.selectable_command_item(list.addon, list.item(row)),
                    Source::Snippets { filtered, .. } => filtered.get(row).is_some(),
                    Source::ScriptFilter { results, .. } => results
                        .as_ref()
                        .is_some_and(|result| result.items.get(row).is_some()),
                };
                consider(selectable);
            }
            for _ in &self.filtered_configs {
                consider(true);
            }
            for (source, row) in self.main_view.rows(Block::AfterLaunchers) {
                let selectable = match &self.main_view.sources[source] {
                    Source::List(list) => self.selectable_command_item(list.addon, list.item(row)),
                    Source::Snippets { filtered, .. } => filtered.get(row).is_some(),
                    Source::ScriptFilter { results, .. } => results
                        .as_ref()
                        .is_some_and(|result| result.items.get(row).is_some()),
                };
                consider(selectable);
            }
            for _ in 0..self.fallback_count() {
                consider(true);
            }

            (selectable_count, candidate)
        };
        if selectable_count == 1 {
            candidate
        } else {
            None
        }
    }

    fn selectable_command_item(
        &self,
        addon: Option<CommandListAddon>,
        item: Option<&super::command_list::CommandItem>,
    ) -> bool {
        let (Some(addon), Some(item)) = (addon, item) else {
            return false;
        };
        if let Some(vault) = VaultAddon::of(addon) {
            let identity = item.identity.as_str();
            return match vault {
                VaultAddon::Pass => {
                    identity == super::pass::LOCK_IDENTITY
                        || identity == super::pass::SYNC_IDENTITY
                        || super::pass::entry_name(identity).is_some()
                }
                VaultAddon::Bitwarden => {
                    identity == super::bitwarden::LOCK_IDENTITY
                        || identity == super::bitwarden::SYNC_IDENTITY
                        || super::bitwarden::item_id(identity).is_some()
                }
            };
        }
        item.unavailable.is_none() && item.action(self.view.current_modifiers).is_some()
    }

    /// Look up the page preview of the selected browser tab, one at a time.
    fn ensure_tab_preview(&mut self) -> Task<Message> {
        let tab = (self.view.show_preview
            && self.command_list.addon == Some(CommandListAddon::BrowserTabs))
        .then(|| {
            self.selected_index
                .checked_sub(self.command_list_offset())
                .and_then(|row| self.command_list.item(row))
                .and_then(super::browser_tabs::tab_target)
        })
        .flatten();
        let service = self.addons.browser_tabs.favicon_service;
        let Some(tab) = tab.cloned() else {
            self.command_list.preview_wanted.clear();
            return self.ensure_main_view_tab_preview(service);
        };
        request_tab_preview(&mut self.command_list, &tab, service)
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        if self.vault.prompt.is_some() {
            match message {
                Message::Submit => return self.submit_vault_prompt(),
                Message::Cancel => return self.cancel_vault_prompt(),
                ref message if is_user_input(message) => return Task::none(),
                _ => {}
            }
        }
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
                // Escape stops an unlock, a slow fetch or a sync and returns
                // to the list, or leaves the mode if it never loaded.
                let unlocking = self.vault.unlocking;
                if let Some(kind) = self
                    .vault
                    .running
                    .as_ref()
                    .map(|op| op.kind.clone())
                    .filter(|kind| unlocking || matches!(kind, OpKind::Entry(..) | OpKind::Sync))
                {
                    self.vault.abort();
                    if kind == OpKind::Load {
                        return self.handle_search_changed(String::new());
                    }
                    return Task::none();
                }
                iced::exit()
            }
            Message::PromptChanged(value) => {
                let busy = self.vault.busy();
                if let Some(prompt) = self.vault.prompt.as_mut().filter(|_| !busy) {
                    prompt.buffer = value;
                }
                Task::none()
            }
            Message::VaultDone(generation, report) => self.handle_vault_done(generation, report),
            Message::VaultSearchDone(generation, identities) => {
                self.handle_vault_search_done(generation, identities);
                Task::none()
            }
            Message::VaultShortcut(action) => self.handle_vault_shortcut(action),
            Message::FileBrowserOpenSelected if self.vault_addon().is_some() => {
                self.handle_vault_shortcut(EntryAction::Copy(Field::Otp))
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
            Message::ScriptFilterPreviewDue(generation, index) => {
                self.handle_script_filter_preview_due(generation, index)
            }
            Message::ScriptFilterPreview(generation, config_index, index, result) => {
                self.handle_script_filter_preview(generation, config_index, index, result)
            }
            Message::FileBrowserPreviewDue(path) => self.handle_file_browser_preview_due(path),
            Message::FileBrowserPreview(key, result) => {
                self.handle_file_browser_preview(key, result)
            }
            Message::TextSnippetCommandResult(generation, result) => {
                self.handle_text_snippet_command_result(generation, result)
            }
            Message::TextSnippetSelected(index) => self.handle_text_snippet_selected(index),
            Message::TextSnippetClipboard(session, result) if session & MAIN_VIEW_SESSION != 0 => {
                self.handle_main_view_clipboard(session, result)
            }
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
            Message::MainViewCycle(backwards) => self.handle_main_view_cycle(backwards),
            Message::FileBrowserToggleHidden => self.handle_file_browser_toggle_hidden(),
            Message::FileBrowserCopyPath => self.handle_file_browser_copy_path(),
            Message::FileBrowserCopyImage => self.handle_file_browser_copy_image(),
            Message::FileBrowserCopyCompleted(path, result) => {
                if self
                    .selected_file_browser_entry()
                    .is_some_and(|entry| entry.full_path == path)
                {
                    self.file_browser.error = result.err();
                }
                Task::none()
            }
            Message::FileBrowserOpenSelected => self.handle_file_browser_open_selected(),
            Message::FileBrowserOpenFileManager => self.handle_file_browser_open_file_manager(),
            Message::FileBrowserCycleSort => self.handle_file_browser_cycle_sort(),
            Message::FileBrowserCycleShow => self.handle_file_browser_cycle_show(),
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
                self.request_script_filter_preview()
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
            Message::MainViewLoaded(source, generation, result) => {
                self.handle_main_view_loaded(source, generation, result)
            }
            Message::MainViewSnippetsLoaded(source, result) => {
                self.handle_main_view_snippets_loaded(source, result)
            }
            Message::MainViewScriptFilter(source, generation, result) => {
                self.handle_main_view_script_filter(source, generation, result)
            }
            Message::TabPreviewLoaded(session, url, bodies) if session & MAIN_VIEW_SESSION != 0 => {
                self.handle_main_view_tab_preview(session, url, bodies)
            }
            Message::TabPreviewLoaded(session, url, bodies) => {
                if session == self.command_list.session {
                    if self.command_list.preview_inflight.as_ref() == Some(&url) {
                        self.command_list.preview_inflight = None;
                    }
                    if let Some(bodies) = bodies {
                        self.command_list.previews.insert(url, bodies);
                    }
                }
                Task::none()
            }
            Message::FaviconsFetched(session, fetched) if session & MAIN_VIEW_SESSION != 0 => {
                self.handle_main_view_favicons(session, fetched)
            }
            Message::FaviconsFetched(session, fetched) => {
                if fetched
                    && self.command_list.addon == Some(CommandListAddon::BrowserTabs)
                    && self.command_list.session == session
                {
                    self.load_command_list(super::browser_tabs::load_tabs, true)
                } else {
                    Task::none()
                }
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
            Message::ClipboardDelete => self.handle_clipboard_delete(),
            Message::ClipboardDeleteDone(session, result) => {
                self.handle_clipboard_delete_done(session, result)
            }
        }
    }

    pub(super) fn handle_search_changed(&mut self, query: String) -> Task<Message> {
        if self.view.current_modifiers.alt() && !self.history.search_in_progress {
            return Task::none();
        }
        // Ctrl+. cycles the file browser filter, but the text input still
        // types the ".", since only control characters are skipped there.
        if self.view.current_modifiers.control()
            && self.file_browser.active
            && is_single_insert(&self.search_query, &query, '.')
        {
            return Task::none();
        }

        let fido = self.addons.file_browser.enabled
            && self.addons.file_browser.fido_style
            && self.file_browser.active
            && !self.history.search_in_progress
            && !self.view.help_active;
        let mut restarted = false;
        let (query, left_dir) = if !fido {
            (query, None)
        } else if let Some((parent, left)) = fido_updir(
            &self.search_query,
            &query,
            &std::env::var("HOME").unwrap_or_default(),
        ) {
            (parent, Some(left))
        } else if let Some(restart) = fido_restart(&self.search_query, &query) {
            restarted = true;
            (restart, None)
        } else {
            (query, None)
        };

        if self.select_1_query.is_some() {
            if self.select_1_query.as_deref() == Some(query.as_str()) && !self.view.help_active {
                self.select_1_ready = true;
            } else {
                self.select_1_query = None;
                self.select_1_ready = false;
            }
        }

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
        self.main_view.active = matches!(class.mode, QueryMode::Standard);
        // Ctrl+Tab on one source lists only its rows.
        let focused = self.main_view.focused().is_some();
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
        }
        .filter(|_| !focused);
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
        self.keyword_suggestions = if matches!(qmode, QueryMode::Standard) && !focused {
            keyword_suggestions(&query, &self.addons)
        } else {
            Vec::new()
        };
        let main_view_query = if matches!(qmode, QueryMode::Standard) {
            query.trim()
        } else {
            ""
        };
        tasks.push(self.refresh_main_view(main_view_query));
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
                    if restarted {
                        tasks.push(move_cursor_to_end(self.view.search_input_id.clone()));
                    }
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
                tasks.push(self.snap_to_selection());
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

        if focused {
            self.currency.clear();
        } else if let Some(task) = self.handle_currency_query(&query, &mut tasks) {
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

        let show = self.file_browser.show;
        self.file_browser.hidden_match = None;
        if filter_text.is_empty() {
            self.file_browser.entries = self
                .file_browser
                .all_entries
                .iter()
                .filter(|entry| show.includes(entry.is_dir))
                .cloned()
                .collect();
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
            let (visible, hidden): (Vec<_>, Vec<_>) = scored
                .into_iter()
                .map(|(index, _)| self.file_browser.all_entries[index].clone())
                .partition(|entry| show.includes(entry.is_dir));
            self.file_browser.entries = visible;
            self.file_browser.hidden_match = hidden.into_iter().next();
        }

        if self.file_browser.error.is_none()
            && filter_text.is_empty()
            && self.file_browser.entries.is_empty()
            && show != crate::FileBrowserShow::All
            && Path::new(dir_path).is_dir()
        {
            self.file_browser.error = Some(match show {
                crate::FileBrowserShow::Directories => {
                    "No directories here (Ctrl+. to show all)".to_string()
                }
                _ => "No files here (Ctrl+. for directories)".to_string(),
            });
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
            self.script_filter.config_index = Some(config_index);
            self.script_filter.query.clone_from(&query);
            self.script_filter.previews.clear();
            self.script_filter.bump_preview_generation();
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

        type Reader = fn(&str) -> std::result::Result<Vec<crate::TextSnippet>, String>;
        let path_source = config
            .file
            .as_ref()
            .map(|path| (path, read_snippet_file as Reader))
            .or_else(|| {
                config
                    .directory
                    .as_ref()
                    .map(|path| (path, read_snippet_dir as Reader))
            });
        if let Some((path, read)) = path_source {
            if let Some(cached) = self.text_snippets.file_cache.get(path).cloned() {
                self.text_snippets.items = cached;
            } else {
                match read(path) {
                    Ok(snippets) => {
                        self.text_snippets
                            .file_cache
                            .insert(path.clone(), snippets.clone());
                        self.text_snippets.items = snippets;
                    }
                    Err(error) => {
                        eprintln!("Text snippets: {error}");
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
    pub(super) fn finish_text_snippet_load(&mut self, query: &str, tasks: &mut Vec<Task<Message>>) {
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
                        .set_notice("Window switching needs Hyprland, niri or sway".to_string());
                    return Task::none();
                };
                if !(self.program_exists)(compositor.program()) {
                    self.command_list
                        .set_notice(format!("{} not found", compositor.program()));
                    return Task::none();
                }
                self.load_command_list(move || compositor.load_windows(), false)
            }
            CommandListAddon::BrowserTabs if !entered => Task::none(),
            CommandListAddon::BrowserTabs => {
                let load = self.load_command_list(super::browser_tabs::load_tabs, false);
                let session = self.command_list.session;
                let service = self.addons.browser_tabs.favicon_service;
                load.chain(Task::perform(
                    async move {
                        let (sender, receiver) = iced::futures::channel::oneshot::channel();
                        std::thread::spawn(move || {
                            let _ = sender.send(super::favicons::fetch_pending(service));
                        });
                        receiver.await.unwrap_or(false)
                    },
                    move |fetched| Message::FaviconsFetched(session, fetched),
                ))
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
            CommandListAddon::Terminal if !entered => Task::none(),
            CommandListAddon::Terminal => {
                let env_terminal = std::env::var("TERMINAL").ok();
                let Some(prefix) = crate::terminal::resolve(
                    self.addons.terminal.command.as_deref(),
                    env_terminal.as_deref(),
                    &self.program_exists,
                ) else {
                    self.command_list
                        .set_notice("No terminal found, set addons.terminal.command".to_string());
                    return Task::none();
                };
                let shell = self.view.default_script_shell.clone();
                let hold = self.addons.terminal.hold;
                let tab = self.terminal_tab_target(&prefix);
                let icon = self.resolve_icon_name(super::terminal::TERMINAL_ICON);
                self.command_list.query_row = Some(std::sync::Arc::new(move |query: &str| {
                    let mut item =
                        super::terminal::run_item(query, &prefix, &shell, hold, tab.as_ref());
                    item.icon = icon.clone();
                    Some(item)
                }));
                self.set_command_list_items(Vec::new());
                Task::none()
            }
            CommandListAddon::Bitwarden if !entered => self.search_vault_notes(),
            CommandListAddon::Pass if !entered => Task::none(),
            CommandListAddon::Pass | CommandListAddon::Bitwarden => {
                let exists = self.program_exists;
                let missing = match addon {
                    CommandListAddon::Pass if !exists("pass") => Some("pass"),
                    CommandListAddon::Pass if !exists("gpg") && !exists("gpg2") => Some("gpg"),
                    CommandListAddon::Bitwarden if !exists("rbw") => Some("rbw"),
                    _ => None,
                };
                if let Some(tool) = missing {
                    self.command_list.set_notice(format!("{tool} not found"));
                    return Task::none();
                }
                let Some(vault) = VaultAddon::of(addon) else {
                    return Task::none();
                };
                self.command_list.usage = self.vault_usage();
                self.command_list.begin_load();
                self.start_vault(
                    Op {
                        addon: vault,
                        kind: OpKind::Load,
                    },
                    None,
                )
            }
        }
    }

    /// How much each password manager row was used, by identity.
    pub(super) fn vault_usage(&self) -> HashMap<String, u64> {
        let map = &self.vault.usage;
        let (max_count, min_ts, max_ts) = mru_bounds(map);
        map.keys()
            .map(|key| {
                let score = mru_sort_key(key, map, &self.sort_mode, max_count, min_ts, max_ts);
                (key.clone(), score)
            })
            .collect()
    }

    /// The password manager whose list is shown, if any.
    pub(super) fn vault_addon(&self) -> Option<VaultAddon> {
        self.command_list.addon.and_then(VaultAddon::of)
    }

    /// Drop the password manager operation once its list is left, and show
    /// its prompt or progress over the list.
    fn sync_vault(&mut self) {
        let active = self.vault_addon();
        let pending = self
            .vault
            .prompt
            .as_ref()
            .map(|prompt| prompt.op.addon)
            .or_else(|| self.vault.running.as_ref().map(|op| op.addon));
        if pending.is_some() && pending != active {
            self.vault.abort();
        }
        if active != Some(VaultAddon::Bitwarden) {
            self.vault.stop_search();
        }
        self.command_list.overlay = active.and_then(|_| self.vault.overlay());
    }

    /// Search Bitwarden notes, URIs and custom fields for the filter after a
    /// pause in typing, once the list is shown.
    fn search_vault_notes(&mut self) -> Task<Message> {
        let query = self.command_list.query.clone();
        if self.vault.search_query.as_ref() == Some(&query)
            || self
                .command_list
                .extra
                .as_ref()
                .is_some_and(|(searched, _)| *searched == query)
        {
            return Task::none();
        }
        self.vault.stop_search();
        let term = query.trim().to_string();
        if self.vault_addon() != Some(VaultAddon::Bitwarden)
            || term.chars().count() < super::bitwarden::SEARCH_MIN_CHARS
            || self.command_list.loading
            || self.command_list.notice.is_some()
            || self.vault.busy()
        {
            return Task::none();
        }
        let (generation, cancel) = self.vault.begin_search(query);
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    std::thread::sleep(super::bitwarden::SEARCH_DEBOUNCE);
                    let found = if cancel.is_cancelled() {
                        Default::default()
                    } else {
                        let runner = SystemRunner { cancel };
                        super::bitwarden::search(&runner, &term).unwrap_or_else(|error| {
                            crate::debug_log!("vault: notes search failed: {}", error.message());
                            Default::default()
                        })
                    };
                    let _ = sender.send(found);
                });
                receiver.await.unwrap_or_default()
            },
            move |found| Message::VaultSearchDone(generation, found),
        )
    }

    fn handle_vault_search_done(
        &mut self,
        generation: u64,
        identities: std::collections::HashSet<String>,
    ) {
        let Some(query) = self.vault.finish_search(generation) else {
            return;
        };
        if self.vault_addon() != Some(VaultAddon::Bitwarden) {
            return;
        }
        crate::debug_log!("vault: notes search found {}", identities.len());
        self.command_list.set_extra(query, identities);
    }

    /// Start `op` off the UI thread, with what the user typed in the prompt
    /// if it follows one.
    fn start_vault(&mut self, op: Op, credential: Option<Secret>) -> Task<Message> {
        self.run_vault(op, credential, false)
    }

    /// Start `op`, after `rbw unlock` when `unlock_first` is set.
    fn run_vault(
        &mut self,
        op: Op,
        credential: Option<Secret>,
        unlock_first: bool,
    ) -> Task<Message> {
        let addon = op.addon;
        let request = Request {
            backend: match addon {
                VaultAddon::Pass => Backend::Pass(PassContext::new(&self.addons.pass)),
                VaultAddon::Bitwarden => Backend::Bitwarden,
            },
            settings: match addon {
                VaultAddon::Pass => JobSettings {
                    clipboard_timeout: self.addons.pass.clipboard_timeout(),
                    type_delay: Duration::from_millis(self.addons.pass.type_delay_ms()),
                },
                VaultAddon::Bitwarden => JobSettings {
                    clipboard_timeout: self.addons.bitwarden.clipboard_timeout(),
                    type_delay: Duration::from_millis(self.addons.bitwarden.type_delay_ms()),
                },
            },
            op: op.clone(),
            credential,
            unlock_first,
        };
        crate::debug_log!("vault: starting {}", op.log_name());
        // A search racing with Lock could ask for the master password again.
        self.vault.stop_search();
        let (generation, cancel) = self.vault.begin(op);
        self.vault.unlocking = unlock_first;
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let runner = SystemRunner { cancel };
                    let _ = sender.send(super::vault::run(&runner, request));
                });
                receiver.await.unwrap_or_else(|_| {
                    Report::failed(SecretError::Tool("stopped unexpectedly".to_string()))
                })
            },
            move |report| Message::VaultDone(generation, report),
        )
    }

    fn submit_vault_prompt(&mut self) -> Task<Message> {
        if self.vault.busy() {
            return Task::none();
        }
        let Some(prompt) = self.vault.prompt.as_mut() else {
            return Task::none();
        };
        if prompt.buffer.is_empty() {
            return Task::none();
        }
        let credential = std::mem::take(&mut prompt.buffer);
        prompt.error = None;
        let op = prompt.op.clone();
        self.start_vault(op, Some(credential))
    }

    /// Close the prompt; leave the mode when the list never loaded.
    fn cancel_vault_prompt(&mut self) -> Task<Message> {
        let loading = self
            .vault
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.op.kind == OpKind::Load);
        self.vault.abort();
        if loading {
            return self.handle_search_changed(String::new());
        }
        Task::none()
    }

    fn handle_vault_done(&mut self, generation: u64, report: Report) -> Task<Message> {
        let Some(op) = self.vault.finish(generation) else {
            crate::debug_log!("vault: dropping a stale result");
            return Task::none();
        };
        if !matches!(report.outcome, Outcome::Prompt(..)) {
            self.vault.prompt = None;
        }
        // Uncover the rows so the rows added below keep the selection.
        self.sync_vault();
        let ready = matches!(report.outcome, Outcome::Ready(_));
        if let Some(notice) = &report.notice {
            if ready {
                super::hyprland::notify(notice);
            } else {
                self.set_command_list_front(super::command_list::CommandItem::info(
                    "notice",
                    notice.clone(),
                    "dialog-information",
                ));
            }
        }
        match report.outcome {
            Outcome::Loaded(items) => {
                let selected = self
                    .command_list
                    .item(self.selected_index)
                    .map(|item| item.identity.clone());
                self.set_command_list_items(items);
                self.selected_index = selected
                    .and_then(|identity| self.command_list.position(&identity))
                    .unwrap_or(0);
                self.search_vault_notes()
            }
            Outcome::Ready(job) => {
                crate::debug_log!("vault: running {} after close", op.log_name());
                self.save_query_to_history();
                if let Some(identity) = op.identity() {
                    record_mru_use(&mut self.vault.usage, identity.clone());
                    save_mru_map(self.vault.usage_path.as_deref(), &self.vault.usage);
                    if op.addon == VaultAddon::Pass {
                        self.record_recent(RecentPick::new("pass".to_string(), identity));
                    }
                }
                if let Ok(mut slot) = self.deferred_action.lock() {
                    *slot = Some(super::actions::DeferredAction::Secret(job));
                }
                iced::exit()
            }
            Outcome::Done if op.kind == OpKind::Lock => iced::exit(),
            Outcome::Done => {
                self.command_list.begin_refresh();
                self.start_vault(
                    Op {
                        addon: op.addon,
                        kind: OpKind::Load,
                    },
                    None,
                )
            }
            Outcome::NeedsUnlock => self.run_vault(op, None, true),
            Outcome::Prompt(purpose, error) => {
                self.vault.prompt = Some(PromptState {
                    purpose,
                    op,
                    buffer: Secret::default(),
                    error: error.map(|error| error.message()),
                });
                Task::none()
            }
            Outcome::Failed(SecretError::Cancelled) => Task::none(),
            Outcome::Failed(error) => {
                crate::debug_log!("vault: {} failed: {}", op.log_name(), error.message());
                if op.kind == OpKind::Load {
                    self.command_list.set_notice(error.message());
                } else {
                    self.set_command_list_front(super::command_list::CommandItem::info(
                        "error",
                        error.message(),
                        "dialog-error",
                    ));
                }
                Task::none()
            }
        }
    }

    fn handle_vault_shortcut(&mut self, action: EntryAction) -> Task<Message> {
        match self.vault_addon() {
            Some(addon) => match self.selected_index.checked_sub(self.command_list_offset()) {
                Some(row) => self.select_vault_row(addon, row, Some(action)),
                None => Task::none(),
            },
            None => Task::none(),
        }
    }

    /// Rows shown above the command list, counted like `activate_index`.
    pub(super) fn command_list_offset(&self) -> usize {
        let snippets = if self.text_snippets.loading {
            1
        } else if self.text_snippets.active {
            self.text_snippets.filtered.len()
        } else {
            0
        };
        let emoji = if self.emoji.active {
            self.emoji.filtered.len()
        } else {
            0
        };
        let clipboard = if self.clipboard.active {
            self.clipboard.row_count()
        } else {
            0
        };
        self.keyword_suggestions.len() + self.script_filter_offset() + snippets + emoji + clipboard
    }

    /// Run `action` on the entry at `row`, or the Lock or Sync row. Without
    /// an action, Enter copies the password and Ctrl+Enter types it.
    fn select_vault_row(
        &mut self,
        addon: VaultAddon,
        row: usize,
        action: Option<EntryAction>,
    ) -> Task<Message> {
        let Some(item) = self.command_list.item(row) else {
            return Task::none();
        };
        let identity = item.identity.as_str();
        let (lock, sync, entry) = match addon {
            VaultAddon::Pass => (
                super::pass::LOCK_IDENTITY,
                super::pass::SYNC_IDENTITY,
                super::pass::entry_name(identity),
            ),
            VaultAddon::Bitwarden => (
                super::bitwarden::LOCK_IDENTITY,
                super::bitwarden::SYNC_IDENTITY,
                super::bitwarden::item_id(identity),
            ),
        };
        let kind = if identity == lock && action.is_none() {
            OpKind::Lock
        } else if identity == sync && action.is_none() {
            OpKind::Sync
        } else if let Some(entry) = entry {
            let action = action.unwrap_or(if self.view.current_modifiers.control() {
                EntryAction::Type
            } else {
                EntryAction::Copy(Field::Password)
            });
            OpKind::Entry(action, entry.to_string())
        } else {
            return Task::none();
        };
        if kind == OpKind::Lock {
            // Lock wins over whatever Raffi runs. A pinentry rbw already
            // shows stays open, and answering it unlocks again.
            self.vault.abort();
        } else if self.vault.busy() {
            return Task::none();
        }
        self.start_vault(Op { addon, kind }, None)
    }

    fn load_bluetooth(&mut self, refresh: bool) -> Task<Message> {
        let keep_open = !self.addons.bluetooth.close_on_action;
        self.load_command_list(
            move || Ok(super::bluetooth::load_devices(keep_open)),
            refresh,
        )
    }

    /// The running terminal to open typed commands in a tab of, unless
    /// `open_in: window` asks for windows.
    fn terminal_tab_target(&self, prefix: &[String]) -> Option<crate::terminal::TabTarget> {
        let terminal = &self.addons.terminal;
        if terminal.open_in != crate::TerminalOpenIn::Tab {
            return None;
        }
        (self.terminal_tab)(prefix, terminal.kitty_socket.as_deref())
    }

    fn load_audio(&mut self, refresh: bool) -> Task<Message> {
        let options = super::audio::AudioOptions::of(&self.addons.audio);
        self.command_list.query_row = Some(self.audio_query_row(true));
        self.load_command_list(move || Ok(super::audio::load_devices(options)), refresh)
    }

    /// The "Set output volume" row builder; `bare` accepts a lone number.
    pub(super) fn audio_query_row(&self, bare: bool) -> super::command_list::QueryRow {
        let options = super::audio::AudioOptions::of(&self.addons.audio);
        let icon = self.resolve_icon_name(super::audio::SET_VOLUME_ICON);
        std::sync::Arc::new(move |query: &str| {
            let mut item = super::audio::set_volume_item(query, bare, options)?;
            item.icon = icon.clone();
            Some(item)
        })
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
        let main_view = self.invalidate_main_view(addon);
        if self.command_list.addon != Some(addon) || self.command_list.session != session {
            return main_view;
        }
        let task = match result {
            Ok(()) => match addon {
                CommandListAddon::Media => self.load_media(true),
                CommandListAddon::Audio => self.load_audio(true),
                CommandListAddon::Kill => self.load_command_list(super::kill::load_processes, true),
                CommandListAddon::BrowserTabs => {
                    self.load_command_list(super::browser_tabs::load_tabs, true)
                }
                CommandListAddon::Bluetooth => self.load_bluetooth(true),
                CommandListAddon::System
                | CommandListAddon::Windows
                | CommandListAddon::Pass
                | CommandListAddon::Bitwarden
                | CommandListAddon::Terminal => Task::none(),
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
                // A failed signal usually means the process is gone or was
                // replaced, and a timed out connection may still have
                // changed the device, so the list reloads under the error
                // row.
                let refresh = match addon {
                    CommandListAddon::Kill => {
                        self.load_command_list(super::kill::load_processes, true)
                    }
                    CommandListAddon::BrowserTabs => {
                        self.load_command_list(super::browser_tabs::load_tabs, true)
                    }
                    CommandListAddon::Bluetooth => self.load_bluetooth(true),
                    _ => Task::none(),
                };
                self.set_command_list_front(super::command_list::CommandItem::info(
                    "error",
                    error,
                    "dialog-error",
                ));
                refresh
            }
        };
        Task::batch([main_view, task])
    }

    /// Show `item` above the list. The highlight stays on the same entry, so
    /// the next key never acts on the row that slid under it.
    fn set_command_list_front(&mut self, mut item: super::command_list::CommandItem) {
        item.icon = item
            .icon_names
            .iter()
            .find_map(|name| self.resolve_icon_name(name));
        let offset = self.command_list_offset();
        let selected = self
            .selected_index
            .checked_sub(offset)
            .and_then(|row| self.command_list.item(row))
            .map(|item| item.identity.clone());
        self.command_list.show_error(item);
        if let Some(row) = selected.and_then(|id| self.command_list.position(&id)) {
            self.selected_index = offset + row;
        }
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
    pub(super) fn resolve_icon_name(&self, name: &str) -> Option<String> {
        if Path::new(name).is_absolute() {
            return Path::new(name).is_file().then(|| name.to_string());
        }
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
        Task::batch([
            self.snap_to_selection(),
            self.request_script_filter_preview(),
        ])
    }

    fn snap_to_selection(&self) -> Task<Message> {
        let total = self.total_items();
        let offset = if total > 1 {
            self.selected_index as f32 / (total - 1) as f32
        } else {
            0.0
        };
        snap_to(
            self.view.scrollable_id.clone(),
            scrollable::RelativeOffset { x: 0.0, y: offset },
        )
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

        if self.currency.help {
            if index == current_index {
                return Task::none();
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

        let before = self.main_view.row_count(Block::BeforeLaunchers);
        if index >= current_index && index < current_index + before {
            return self.activate_main_view(Block::BeforeLaunchers, index - current_index);
        }
        current_index += before;

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

        let after = self.main_view.row_count(Block::AfterLaunchers);
        if index >= current_index && index < current_index + after {
            return self.activate_main_view(Block::AfterLaunchers, index - current_index);
        }
        current_index += after;

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
        self.request_script_filter_preview()
    }

    /// Index of the selected Script Filter result when it should get a
    /// dynamic preview.
    pub(super) fn dynamic_preview_target(&self) -> Option<usize> {
        if !self.view.show_preview || self.view.help_active {
            return None;
        }
        let config_index = self.script_filter.config_index?;
        if self
            .script_filter
            .preview_unsupported
            .contains(&config_index)
        {
            return None;
        }
        let config = self.addons.script_filters.get(config_index)?;
        if config.dynamic_preview == Some(false)
            || self.script_filter.loading
            || self.script_filter.help_message.is_some()
        {
            return None;
        }
        let index = self
            .selected_index
            .checked_sub(self.keyword_suggestions.len())?;
        let item = self.script_filter.results.as_ref()?.items.get(index)?;
        let opted_out = item
            .preview
            .as_ref()
            .and_then(super::types::ScriptFilterPreview::as_rich)
            .is_some_and(|preview| !preview.dynamic);
        (!opted_out).then_some(index)
    }

    /// Drop pending preview work and, when the selection is a Script Filter
    /// result, schedule its dynamic preview.
    pub(super) fn request_script_filter_preview(&mut self) -> Task<Message> {
        self.script_filter.bump_preview_generation();
        let Some(index) = self.dynamic_preview_target() else {
            return Task::none();
        };
        let cached_without_refresh = self
            .script_filter
            .previews
            .get(&index)
            .and_then(|dynamic| dynamic.preview.as_ref())
            .is_some_and(|preview| preview.refresh.is_none());
        if cached_without_refresh {
            return Task::none();
        }
        schedule_script_filter_preview(
            SCRIPT_FILTER_PREVIEW_DEBOUNCE,
            self.script_filter.preview_generation,
            self.script_filter.preview_live_generation.clone(),
            index,
        )
    }

    fn handle_script_filter_preview_due(&mut self, generation: u64, index: usize) -> Task<Message> {
        if generation != self.script_filter.preview_generation
            || self.dynamic_preview_target() != Some(index)
        {
            return Task::none();
        }
        let Some(config_index) = self.script_filter.config_index else {
            return Task::none();
        };
        if !self.script_filter.preview_supported.contains(&config_index)
            && !self.script_filter.preview_probing.insert(config_index)
        {
            // A first run is still out; its reply asks again for the selection.
            return Task::none();
        }
        let (Some(config), Some(item)) = (
            self.addons.script_filters.get(config_index),
            self.script_filter
                .results
                .as_ref()
                .and_then(|results| results.items.get(index)),
        ) else {
            self.script_filter.preview_probing.remove(&config_index);
            return Task::none();
        };
        let pane = (
            self.preview_viewport_width().round() as u32,
            self.preview_viewport_height().round() as u32,
        );
        let mut request = PreviewRequest::new(config, &self.script_filter.query, item, pane);
        request.env.extend(self.view.theme.command_env());
        execute_script_filter_preview(request, generation, config_index, index)
    }

    fn handle_script_filter_preview(
        &mut self,
        generation: u64,
        config_index: usize,
        index: usize,
        result: std::result::Result<
            Option<super::types::PreparedPreview>,
            super::types::ScriptFilterPreviewError,
        >,
    ) -> Task<Message> {
        use super::types::ScriptFilterPreviewError;
        // Whether the script speaks the protocol holds even for stale replies.
        let was_probing = self.script_filter.preview_probing.remove(&config_index);
        let supported = self.script_filter.preview_supported.contains(&config_index);
        // Only the first run's non-preview output disables previews; once
        // the script has printed a preview object it is shown as an error.
        let result = match result {
            Ok(None) if supported => Err(ScriptFilterPreviewError::Invalid(
                "Not a preview object".into(),
            )),
            result => result,
        };
        match &result {
            Ok(Some(_)) | Err(ScriptFilterPreviewError::Invalid(_)) => {
                self.script_filter.preview_supported.insert(config_index);
            }
            Ok(None) => {
                crate::debug_log!("script_filter: no preview object, disabling dynamic preview");
                self.script_filter.preview_unsupported.insert(config_index);
                return Task::none();
            }
            // A legacy script may be slow or fail when run for a preview;
            // re-running it on every selection would only repeat that.
            Err(ScriptFilterPreviewError::Run(error)) if !supported => {
                crate::debug_log!(
                    "script_filter: first preview run failed ({error}), disabling dynamic preview"
                );
                self.script_filter.preview_unsupported.insert(config_index);
                return Task::none();
            }
            Err(ScriptFilterPreviewError::Run(_)) => {}
        }
        if generation != self.script_filter.preview_generation {
            // Requests made while the probe ran were skipped; retry the current one.
            return if was_probing {
                self.request_script_filter_preview()
            } else {
                Task::none()
            };
        }
        let entry = match result {
            Ok(Some(preview)) => {
                if preview.file_path().is_some() {
                    // Same path on refresh still needs a fresh read.
                    self.file_browser.cached_preview = None;
                }
                let entry = self.script_filter.previews.entry(index).or_default();
                entry.preview = Some(preview);
                entry.error = None;
                entry
            }
            Ok(None) => return Task::none(),
            Err(error) => {
                let error = error.into_message();
                crate::debug_log!("script_filter: preview failed: {error}");
                let entry = self.script_filter.previews.entry(index).or_default();
                entry.error = Some(error);
                entry
            }
        };
        // Keep refreshing after a failure too, so a transient error recovers.
        let refresh = entry.preview.as_ref().and_then(|preview| preview.refresh);
        self.refresh_file_preview_cache();
        match refresh {
            Some(delay) => schedule_script_filter_preview(
                delay,
                generation,
                self.script_filter.preview_live_generation.clone(),
                index,
            ),
            None => Task::none(),
        }
    }

    pub(super) fn handle_script_filter_selected(&mut self, index: usize) -> Task<Message> {
        let mut pick = None;
        if let Some(result) = &self.script_filter.results {
            if let Some(item) = result.items.get(index) {
                if let Some(config) = self
                    .script_filter
                    .config_index
                    .and_then(|index| self.addons.script_filters.get(index))
                {
                    let mut recent = RecentPick::new(
                        format!("script_filters.{}", config.name),
                        item.arg.clone().unwrap_or_else(|| item.title.clone()),
                    );
                    recent.item = Some(StoredItem::of(item, result.default_icon.as_deref()));
                    pick = Some(recent);
                }
                let value = item.arg.as_deref().unwrap_or(&item.title);
                let action = if self.view.current_modifiers.control() {
                    self.script_filter
                        .secondary_action
                        .as_deref()
                        .or(self.script_filter.action.as_deref())
                } else {
                    self.script_filter.action.as_deref()
                };
                execute_action_with_env(action.unwrap_or("copy"), value, &item.variables);
            }
        }
        if let Some(pick) = pick {
            self.record_recent(pick);
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

    pub(super) fn handle_text_snippet_selected(&mut self, index: usize) -> Task<Message> {
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
            if let Some(config) = self
                .text_snippets
                .source_index
                .and_then(|index| self.addons.text_snippets.get(index))
            {
                let source = format!("text_snippets.{}", config.name);
                self.record_recent(RecentPick::new(source, snippet.name.clone()));
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
                let new_query = format!(
                    "{}/",
                    tilde_path(&entry.full_path, &std::env::var("HOME").unwrap_or_default())
                );
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

    /// Ctrl+Tab: narrow the main view to its next source, or back to all
    /// of them; Tab completion where the main view does not show.
    fn handle_main_view_cycle(&mut self, backwards: bool) -> Task<Message> {
        if self.view.help_active || !self.main_view.active || self.main_view.stops.is_empty() {
            return self.handle_tab_complete();
        }
        self.main_view.cycle(backwards);
        self.handle_search_changed(self.search_query.clone())
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
        let entry = if self.file_browser.entries.is_empty() {
            self.file_browser.hidden_match.as_ref()
        } else {
            self.file_browser.entries.get(entry_index)
        };
        if let Some(entry) = entry {
            let path = tilde_path(&entry.full_path, &std::env::var("HOME").unwrap_or_default());
            let new_query = if entry.is_dir {
                format!("{path}/")
            } else {
                path
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

    fn handle_file_browser_cycle_show(&mut self) -> Task<Message> {
        if !self.file_browser.active {
            return Task::none();
        }
        self.file_browser.show = self.file_browser.show.next();
        let query = self.search_query.clone();
        Task::done(Message::SearchChanged(query))
    }

    fn selected_file_browser_entry(&self) -> Option<&super::state::FileBrowserEntry> {
        if !self.file_browser.active {
            return None;
        }
        let entry_index = self
            .selected_index
            .saturating_sub(self.script_filter_offset());
        self.file_browser.entries.get(entry_index)
    }

    fn handle_file_browser_copy_path(&mut self) -> Task<Message> {
        let Some(entry) = self.selected_file_browser_entry() else {
            return Task::none();
        };
        let path = entry.full_path.clone();
        self.file_browser.error = None;
        let copy = self.copy_job(path.clone());
        checked_clipboard_copy_task(path, move || {
            copy().map_err(|error| format!("Cannot copy path: {error}"))
        })
    }

    fn handle_file_browser_copy_image(&mut self) -> Task<Message> {
        let Some(entry) = self.selected_file_browser_entry() else {
            return Task::none();
        };
        if entry.is_dir {
            self.file_browser.error =
                Some("Cannot copy image: selected entry is a directory".into());
            return Task::none();
        }
        let Some(mime_type) = image_mime_type(Path::new(&entry.full_path)) else {
            self.file_browser.error =
                Some("Cannot copy image: selected entry is not a supported image".into());
            return Task::none();
        };
        let path = entry.full_path.clone();
        self.file_browser.error = None;
        checked_clipboard_copy_task(path.clone(), move || {
            copy_image_checked(Path::new(&path), mime_type)
                .map_err(|error| format!("Cannot copy image: {error}"))
        })
    }

    fn handle_file_browser_open_selected(&mut self) -> Task<Message> {
        let Some(entry) = self.selected_file_browser_entry() else {
            return Task::none();
        };
        execute_file_action("open", &entry.full_path);
        self.save_query_to_history();
        iced::exit()
    }

    fn handle_file_browser_open_file_manager(&mut self) -> Task<Message> {
        let dir = &self.file_browser.current_dir;
        if !self.file_browser.active || dir.is_empty() || !Path::new(dir).is_dir() {
            return Task::none();
        }
        let selected = self
            .selected_file_browser_entry()
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
    pub(super) fn handle_command_list_selected(&mut self, row: usize) -> Task<Message> {
        let Some(addon) = self.command_list.addon else {
            return Task::none();
        };
        if let Some(vault) = VaultAddon::of(addon) {
            return self.select_vault_row(vault, row, None);
        }
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
                self.record_command_list_pick(addon, &identity);
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
                self.save_query_to_history();
                iced::exit()
            }
        }
    }

    /// Keep a tab or window run with its main action as a recent pick;
    /// Ctrl+Enter actions such as closing a tab do not count.
    fn record_command_list_pick(&mut self, addon: CommandListAddon, identity: &str) {
        let Some(name) = super::main_view::recent_list_name(addon) else {
            return;
        };
        let Some(item) = self
            .command_list
            .items
            .iter()
            .find(|item| item.identity == identity)
        else {
            return;
        };
        if !super::main_view::is_mixable(addon, item)
            || (self.view.current_modifiers.control() && item.secondary.is_some())
        {
            return;
        }
        let mut pick = RecentPick::new(name.to_string(), identity.to_string());
        pick.url = super::browser_tabs::tab_target(item).map(|tab| tab.url.clone());
        if addon == CommandListAddon::Windows {
            pick.session = self.main_view.window_session.clone();
        }
        self.record_recent(pick);
    }

    /// Builds the transform rows at once; hashes follow after a short
    /// debounce, off the UI thread.
    fn handle_devtools_query(&mut self, payload: String) -> Task<Message> {
        self.devtools.active = true;
        self.devtools.rows = transform_rows(&payload);
        self.devtools.loading = false;
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
        self.devtools.loading = true;
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
        self.devtools.loading = false;
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

    pub(super) fn script_filter_offset(&self) -> usize {
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
        if self.main_view.hide_launchers
            || matches!(self.main_view.focused(), Some(Focus::Source(_)))
        {
            self.filtered_configs.clear();
        } else if query.is_empty() {
            self.filtered_configs = (0..self.configs.len()).collect();
        } else {
            self.filtered_configs = fuzzy_match_configs(&self.configs, query, log_query);
        }
        self.main_view.launchers_strong = !query.is_empty()
            && self.filtered_configs.iter().any(|&index| {
                let config = &self.configs[index];
                let title = config
                    .description
                    .as_deref()
                    .or(config.binary.as_deref())
                    .unwrap_or_default();
                super::support::word_prefix_match(title, query)
            });
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
        offset += self.main_view.row_count(Block::BeforeLaunchers);
        offset += self.main_view.row_count(Block::AfterLaunchers);
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
            && self.main_view.focused().is_none()
        {
            self.fallbacks.len()
        } else {
            0
        }
    }

    fn resolve_fallbacks(paths: &[String], addons: &AddonsConfig) -> Vec<ResolvedFallback> {
        let mut resolved = Vec::new();
        for path in paths {
            if path == "addons.terminal" {
                let terminal = &addons.terminal;
                match crate::terminal::resolve_default(terminal.command.as_deref()) {
                    Some(prefix) => resolved.push(ResolvedFallback {
                        name: super::terminal::terminal_name(&prefix).to_string(),
                        icon: Some(super::terminal::TERMINAL_ICON.to_string()),
                        action: FallbackAction::Terminal {
                            prefix,
                            hold: terminal.hold,
                        },
                    }),
                    None => {
                        crate::debug_log!("fallback: no terminal found");
                        eprintln!(
                            "Warning: fallback addons.terminal skipped, no terminal found; set addons.terminal.command"
                        );
                    }
                }
                continue;
            }
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
                        "fallback: unsupported addon type '{addon_type}', expected web_searches, script_filters or terminal"
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
                FallbackAction::Terminal { prefix, hold } => {
                    let tab = self.terminal_tab_target(prefix);
                    let action = super::terminal::run_action(
                        prefix,
                        &self.view.default_script_shell,
                        &query,
                        *hold,
                        tab.as_ref(),
                    );
                    if let Ok(mut slot) = self.deferred_action.lock() {
                        *slot = Some(action);
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
            if !self.main_view.stops.is_empty() {
                add_main_view_shortcut(&mut self.help_entries);
            }
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
        if !self.query_class.saved_in_history() {
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
