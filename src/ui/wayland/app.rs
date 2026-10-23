use std::collections::HashMap;
use std::path::Path;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, move_cursor_to_end, scroll_by};
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::Task;

use super::actions::{
    copy_image_checked, copy_text_checked, execute_action, execute_action_with_env,
    execute_file_action, execute_snippet_action, open_in_file_manager, DeferredAction,
};
use super::browser::read_directory;
use super::cache;
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
use super::help::{
    add_cycle_view_shortcut, filter_help_entries, help_entries, help_suggestions, search_tips,
};
use super::main_view::{Block, Focus, Source, MAIN_VIEW_SESSION};
use super::pass::PassContext;
use super::preview::image_mime_type;
use super::recent::{RecentPick, StoredItem};
use super::script_filters::{
    execute_script_filter, execute_script_filter_preview, parse_preview_response,
    parse_script_filter_output, schedule_script_filter_preview, PreviewRequest,
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
            Self::Clipboard { .. } => f.debug_struct("Clipboard").finish_non_exhaustive(),
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
            QueryMode::Clipboard { .. } => {
                format!("{} <redacted>", addons.clipboard.trigger())
            }
            _ if self.sensitive => "<redacted>".to_string(),
            _ => query.trim().to_string(),
        }
    }
}

pub(super) fn classify_query(query: &str, addons: &AddonsConfig) -> QueryClass {
    let query = expand_launcher_keyword(query, addons);
    let query = query.as_ref();
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

/// `query` with a leading launcher keyword replaced by its alias, so
/// `linoux torvalds` routes like `mp linus/linux torvalds`. The
/// alias is not expanded again, so keywords never chain or loop.
pub(super) fn expand_launcher_keyword<'a>(
    query: &'a str,
    addons: &AddonsConfig,
) -> std::borrow::Cow<'a, str> {
    let input = keyword_input(query);
    addons
        .launcher_keywords
        .iter()
        .find_map(|launcher| {
            let rest = input
                .strip_prefix(launcher.keyword.as_str())?
                .strip_prefix(' ')?;
            Some(format!("{} {rest}", launcher.alias.trim_end()).into())
        })
        .unwrap_or(query.into())
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

/// Theme icon names for the keywords; rows show no icon when the theme lacks
/// them.
pub(super) const CLIPBOARD_ICON: &str = "copyq";
pub(super) const SYSTEM_ICON: &str = "system-shutdown";
pub(super) const WINDOWS_ICON: &str = "preferences-system-windows";
pub(super) const BROWSER_TABS_ICON: &str = "web-browser";
pub(super) const BOOKMARKS_ICON: &str = "user-bookmarks";
pub(super) const BROWSER_HISTORY_ICON: &str = "document-open-recent";
pub(super) const MEDIA_ICON: &str = "multimedia-player";
pub(super) const AUDIO_ICON: &str = "audio-speakers";
pub(super) const KILL_ICON: &str = "utilities-system-monitor";
pub(super) const BLUETOOTH_ICON: &str = "bluetooth-active";
pub(super) const PASS_ICON: &str = "dialog-password";
pub(super) const BITWARDEN_ICON: &str = "bitwarden";
pub(super) const DEVTOOLS_ICON: &str = "utilities-terminal";
pub(super) const FILE_BROWSER_ICON: &str = "folder";
pub(super) const EMOJI_ICON: &str = "face-smile";
pub(super) const CALCULATOR_ICON: &str = "accessories-calculator";
pub(super) const COLOR_ICON: &str = "preferences-desktop-color";
pub(super) const WORLD_CLOCK_ICON: &str = "preferences-system-time";
/// Shown for script filters and web searches configured without an icon.
pub(super) const SEARCH_ICON: &str = "system-search";
/// Shown for text snippets configured without an icon.
pub(super) const SNIPPET_ICON: &str = "accessories-text-editor";

/// Emoji shown in place of a keyword row's theme icon when the icon theme
/// lacks it.
pub(super) fn fallback_emoji(icon: &str) -> Option<&'static str> {
    Some(match icon {
        CLIPBOARD_ICON => "📋",
        SYSTEM_ICON => "🔌",
        WINDOWS_ICON => "🪟",
        BROWSER_TABS_ICON => "🌐",
        BROWSER_HISTORY_ICON => "🕘",
        MEDIA_ICON => "🎵",
        AUDIO_ICON => "🔊",
        KILL_ICON => "💀",
        BLUETOOTH_ICON => "🎧",
        PASS_ICON => "🔑",
        BITWARDEN_ICON => "🛡️",
        super::slack::SLACK_ICON => "💬",
        DEVTOOLS_ICON => "💻",
        FILE_BROWSER_ICON => "📁",
        EMOJI_ICON => "😀",
        CALCULATOR_ICON => "🧮",
        COLOR_ICON => "🎨",
        WORLD_CLOCK_ICON => "🕒",
        SEARCH_ICON => "🔍",
        SNIPPET_ICON => "📝",
        _ => return None,
    })
}

/// Whether `message` comes from the user acting on the list or the query,
/// which the password prompt must not let through.
fn is_user_input(message: &Message) -> bool {
    matches!(
        message,
        Message::SearchChanged(_)
            | Message::MoveUp
            | Message::MoveDown
            | Message::ChipMove(_)
            | Message::ChipClicked(_)
            | Message::ChipScroll(_)
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
            | Message::CycleView(_)
            | Message::FileBrowserToggleHidden
            | Message::FileBrowserCopyPath
            | Message::FileBrowserCopyImage
            | Message::FileBrowserOpenSelected
            | Message::FileBrowserOpenFileManager
            | Message::FileBrowserCycleSort
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
            | Message::DeleteSelected
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

    if addons.clipboard.is_enabled() {
        if let Some(query) = extract_keyword_query(input, addons.clipboard.trigger()) {
            return QueryMode::Clipboard { query };
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

    if addons.bookmarks.enabled {
        if let Some(query) = extract_keyword_query(input, addons.bookmarks.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Bookmarks,
                query,
            };
        }
    }

    if addons.browser_history.enabled {
        if let Some(query) = extract_keyword_query(input, addons.browser_history.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::BrowserHistory,
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

    if addons.slack.enabled {
        if let Some(query) = extract_keyword_query(input, addons.slack.trigger()) {
            return QueryMode::CommandList {
                addon: CommandListAddon::Slack,
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
    // Feature words such as "volume" for the audio addon, matched whole so
    // they do not push launchers down while a longer name is being typed.
    let intent = |words: &[&str]| words.iter().any(|word| *word == input_lower);

    let mut suggestions = Vec::new();

    for launcher in &addons.launcher_keywords {
        if matches(&launcher.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: launcher.keyword.clone(),
                subtitle: launcher.description.clone(),
                icon: launcher.icon.clone(),
            });
        }
    }

    for config in &addons.script_filters {
        if matches(&config.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: config.keyword.clone(),
                subtitle: format!("Search {}", config.name),
                icon: config.icon.clone().or(Some(SEARCH_ICON.to_string())),
            });
        }
    }

    for config in &addons.text_snippets {
        if matches(&config.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: config.keyword.clone(),
                subtitle: format!("Browse {}", config.name),
                icon: config.icon.clone().or(Some(SNIPPET_ICON.to_string())),
            });
        }
    }

    if addons.emoji.enabled {
        let trigger = addons.emoji.trigger.as_deref().unwrap_or("emoji");
        if matches(trigger) || intent(&["emoji", "emojis", "smiley", "symbol", "icon", "icons"]) {
            suggestions.push(KeywordSuggestion {
                keyword: trigger.to_string(),
                subtitle: "Browse emoji and icons".to_string(),
                icon: Some(EMOJI_ICON.to_string()),
            });
        }
    }

    if addons.clipboard.is_enabled()
        && (matches(addons.clipboard.trigger())
            || intent(&["clipboard", "copy", "paste", "copied"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.clipboard.trigger().to_string(),
            subtitle: "Browse clipboard history".to_string(),
            icon: Some(CLIPBOARD_ICON.to_string()),
        });
    }

    for config in &addons.web_searches {
        if matches(&config.keyword) {
            suggestions.push(KeywordSuggestion {
                keyword: config.keyword.clone(),
                subtitle: format!("Search {}", config.name),
                icon: config.icon.clone().or(Some(SEARCH_ICON.to_string())),
            });
        }
    }

    if addons.file_browser.enabled && intent(&["file", "files", "folder", "folders", "directory"]) {
        suggestions.push(KeywordSuggestion {
            keyword: "~/".to_string(),
            subtitle: "Browse files, or type a path such as /etc/".to_string(),
            icon: Some(FILE_BROWSER_ICON.to_string()),
        });
    }

    if addons.system.enabled
        && (matches(addons.system.trigger())
            || intent(&[
                "lock",
                "logout",
                "suspend",
                "sleep",
                "hibernate",
                "reboot",
                "shutdown",
                "poweroff",
                "power",
            ]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.system.trigger().to_string(),
            subtitle: "Lock, suspend, hibernate, log out, reboot or shut down".to_string(),
            icon: Some(SYSTEM_ICON.to_string()),
        });
    }

    if addons.windows.enabled
        && (matches(addons.windows.trigger()) || intent(&["window", "windows"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.windows.trigger().to_string(),
            subtitle: "Switch to an open window".to_string(),
            icon: Some(WINDOWS_ICON.to_string()),
        });
    }

    if addons.browser_tabs.enabled
        && (matches(addons.browser_tabs.trigger()) || intent(&["tab", "tabs", "browser"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.browser_tabs.trigger().to_string(),
            subtitle: "Switch to or close a browser tab".to_string(),
            icon: Some(BROWSER_TABS_ICON.to_string()),
        });
    }

    if addons.bookmarks.enabled
        && (matches(addons.bookmarks.trigger()) || intent(&["bookmark", "bookmarks"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.bookmarks.trigger().to_string(),
            subtitle: "Open a browser bookmark".to_string(),
            icon: Some(BOOKMARKS_ICON.to_string()),
        });
    }

    if addons.browser_history.enabled
        && (matches(addons.browser_history.trigger())
            || intent(&["history", "visited", "recent", "browser"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.browser_history.trigger().to_string(),
            subtitle: "Open a page from browser history".to_string(),
            icon: Some(BROWSER_HISTORY_ICON.to_string()),
        });
    }

    if addons.media.enabled
        && (matches(addons.media.trigger())
            || intent(&["music", "play", "pause", "player", "song", "track"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.media.trigger().to_string(),
            subtitle: "Play, pause or skip in media players".to_string(),
            icon: Some(MEDIA_ICON.to_string()),
        });
    }

    if addons.audio.enabled
        && (matches(addons.audio.trigger())
            || intent(&[
                "volume",
                "sound",
                "mute",
                "unmute",
                "speaker",
                "speakers",
                "microphone",
                "mic",
            ]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.audio.trigger().to_string(),
            subtitle: "Switch or mute audio outputs and inputs".to_string(),
            icon: Some(AUDIO_ICON.to_string()),
        });
    }

    if addons.kill.enabled && (matches(addons.kill.trigger()) || intent(&["process", "processes"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.kill.trigger().to_string(),
            subtitle: "Stop one of your processes".to_string(),
            icon: Some(KILL_ICON.to_string()),
        });
    }

    if addons.bluetooth.enabled
        && (matches(addons.bluetooth.trigger())
            || intent(&["bluetooth", "headphones", "headset", "earbuds"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.bluetooth.trigger().to_string(),
            subtitle: "Connect Bluetooth devices or turn Bluetooth on and off".to_string(),
            icon: Some(BLUETOOTH_ICON.to_string()),
        });
    }

    if addons.pass.enabled
        && (matches(addons.pass.trigger()) || intent(&["password", "passwords", "otp"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.pass.trigger().to_string(),
            subtitle: "Copy or type passwords from pass".to_string(),
            icon: Some(PASS_ICON.to_string()),
        });
    }

    if addons.bitwarden.enabled
        && (matches(addons.bitwarden.trigger())
            || intent(&["bitwarden", "password", "passwords", "otp", "vault"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.bitwarden.trigger().to_string(),
            subtitle: "Copy or type passwords from Bitwarden".to_string(),
            icon: Some(BITWARDEN_ICON.to_string()),
        });
    }

    if addons.slack.enabled && (matches(addons.slack.trigger()) || intent(&["slack", "chat"])) {
        suggestions.push(KeywordSuggestion {
            keyword: addons.slack.trigger().to_string(),
            subtitle: "Open Slack conversations, search messages or set your status".to_string(),
            icon: Some(super::slack::SLACK_ICON.to_string()),
        });
    }

    if addons.devtools.enabled
        && (matches(addons.devtools.trigger())
            || intent(&[
                "base64",
                "encode",
                "decode",
                "hash",
                "json",
                "sha256",
                "md5",
                "urlencode",
            ]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.devtools.trigger().to_string(),
            subtitle: "Encode, decode, hash or reformat text".to_string(),
            icon: Some(DEVTOOLS_ICON.to_string()),
        });
    }

    if addons.terminal.enabled
        && (matches(addons.terminal.trigger()) || intent(&["terminal", "shell"]))
    {
        suggestions.push(KeywordSuggestion {
            keyword: addons.terminal.trigger().to_string(),
            subtitle: "Run a command in a terminal".to_string(),
            icon: Some(super::terminal::TERMINAL_ICON.to_string()),
        });
    }

    suggestions
}

/// Whether typing `?` alone opens the keyword help, which it does unless a
/// configured keyword or trigger is `?`.
pub(super) fn question_mark_opens_help(addons: &AddonsConfig) -> bool {
    matches!(route_query("?", addons), QueryMode::Standard)
        && keyword_suggestions("?", addons).is_empty()
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
            let mut paths: Vec<Option<String>> = configs
                .iter()
                .map(|config| resolve_launcher_icon_path(config, &icon_map, &desktop_icon_map))
                .collect();
            super::thumbnails::substitute(&mut paths);
            paths
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
                slack: Default::default(),
                bookmarks: Default::default(),
                bookmark_searches: Default::default(),
                browser_history: Default::default(),
                clipboard: ClipboardState::default(),
                fallbacks,
                main_view: Default::default(),
                keyword_suggestions: Vec::new(),
                keyword_chips: false,
                chip_index: 0,
                help_entries: Vec::new(),
                help_filtered: Vec::new(),
                help_saved_query: None,
                drag: None,
                close_on_action: None,
                print_only: false,
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

    pub(super) fn should_close(&self, addon_close: Option<bool>) -> bool {
        addon_close.or(self.close_on_action).unwrap_or(true)
    }

    pub(super) fn finish_or_keep_open(&mut self, addon_close: Option<bool>) -> Task<Message> {
        self.save_query_to_history();
        if self.should_close(addon_close) {
            iced::exit()
        } else {
            Task::none()
        }
    }

    pub(super) fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.update_inner(message);
        self.sync_vault();
        let select_1 = self.maybe_auto_select_1();
        let preview = self.ensure_clipboard_preview();
        let file_preview = self.ensure_file_browser_preview();
        let tab_preview = self.ensure_tab_preview();
        let slack_images = self.ensure_slack_images();
        Task::batch([
            task,
            select_1,
            preview,
            file_preview,
            tab_preview,
            slack_images,
        ])
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

            for _ in 0..self.keyword_rows() {
                consider(true);
            }
            if self.script_filter.loading {
                consider(false);
            } else if let Some(result) = &self.script_filter.results {
                for item in &result.items {
                    consider(item.valid);
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
                        .is_some_and(|result| result.items.get(row).is_some_and(|item| item.valid)),
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
                        .is_some_and(|result| result.items.get(row).is_some_and(|item| item.valid)),
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
        if self.view.show_preview && self.command_list.addon == Some(CommandListAddon::Slack) {
            return self.ensure_slack_preview();
        }
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
        if self.bookmarks.prompt.is_some() && !self.view.help_active {
            match message {
                Message::SearchChanged(term) => return self.handle_search_prompt_input(term),
                Message::Cancel => return self.leave_search_prompt(),
                Message::CycleView(_) => return Task::none(),
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
            Message::MouseReleased => {
                // Every left release ends row press tracking, even the ones
                // that happen outside the file browser rows.
                self.file_browser.pressed_row = None;
                Task::none()
            }
            Message::WaylandHandles(handles) => self.handle_wayland_handles(handles),
            Message::DragEvent(event) => self.handle_drag_event(event),
            Message::TabComplete => self.handle_tab_complete(),
            Message::CycleView(backwards) => self.handle_cycle_view(backwards),
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
            Message::MainViewSlackSearchDue(source, generation, query) => {
                self.handle_main_view_slack_search_due(source, generation, query)
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
            Message::SlackSearchDue(session, terms) => self.handle_slack_search_due(session, terms),
            Message::SlackPreviewLoaded(session, identity, request, preview) => {
                self.handle_slack_preview_loaded(session, identity, request, preview)
            }
            Message::SlackImagesDue(session, request) => {
                self.handle_slack_images_due(session, request)
            }
            Message::SlackImagesLoaded(session, epoch, request, keys, outcome) => {
                self.handle_slack_images_loaded(session, epoch, request, keys, outcome)
            }
            Message::ListScrolled(offset) => {
                self.view.list_scrolled = offset > 0.0;
                // Slack downloads the pictures of the rows scrolled into view.
                if self.command_list.addon == Some(CommandListAddon::Slack) {
                    self.slack.scroll = offset;
                }
                Task::none()
            }
            Message::SlackPreviewOpen {
                desktop_url,
                browser_url,
            } => self.handle_slack_preview_open(desktop_url, browser_url),
            Message::FaviconsFetched(session, fetched) if session & MAIN_VIEW_SESSION != 0 => {
                self.handle_main_view_favicons(session, fetched)
            }
            Message::BookmarksLoaded(generation, library) => {
                self.handle_bookmarks_loaded(generation, library)
            }
            Message::BookmarkIconsLoaded(session, icons, fetch) => {
                self.handle_bookmark_icons_loaded(session, icons, fetch)
            }
            Message::BrowserHistoryInitialized(generation, init) => {
                self.handle_browser_history_initialized(generation, init)
            }
            Message::BrowserHistoryGroupLoaded(generation, key, library) => {
                self.handle_browser_history_group_loaded(generation, key, library)
            }
            Message::BrowserHistoryIconsLoaded(session, key, icons, fetch) => {
                self.handle_browser_history_icons_loaded(session, key, icons, fetch)
            }
            Message::BrowserHistorySearchDue(session, generation) => {
                self.handle_browser_history_search_due(session, generation)
            }
            Message::FaviconsFetched(session, true)
                if self.command_list.addon == Some(CommandListAddon::Bookmarks) =>
            {
                self.handle_bookmark_favicons_fetched(session)
            }
            Message::FaviconsFetched(session, true)
                if self.command_list.addon == Some(CommandListAddon::BrowserHistory) =>
            {
                self.handle_browser_history_favicons_fetched(session)
            }
            Message::FaviconsFetched(session, fetched) => {
                if fetched
                    && self.command_list.addon == Some(CommandListAddon::BrowserTabs)
                    && self.command_list.session == session
                {
                    self.load_browser_tabs(true)
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
            Message::ChipMove(backwards) => self.handle_chip_move(backwards),
            Message::ChipScroll(backwards) => {
                self.selected_index = 0;
                self.handle_chip_move(backwards)
            }
            Message::ChipClicked(chip) => {
                self.chip_index = chip;
                self.selected_index = 0;
                self.handle_keyword_suggestion_selected(0)
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
            Message::ClipboardRefresh => self.handle_refresh(),
            Message::DeleteSelected if self.clipboard.active => self.handle_clipboard_delete(),
            Message::DeleteSelected => self.handle_command_list_close(),
            Message::WindowsChanged => self.handle_windows_changed(),
            Message::LauncherRaiseChecked(index, focus) => {
                self.handle_launcher_raise_checked(index, focus)
            }
            Message::ClipboardDeleteDone(session, result) => {
                self.handle_clipboard_delete_done(session, result)
            }
        }
    }

    /// Reload the active view on `Ctrl+R`.
    pub(super) fn handle_refresh(&mut self) -> Task<Message> {
        if self.view.help_active || self.vault.prompt.is_some() {
            return Task::none();
        }
        if self.clipboard.active {
            return self.handle_clipboard_refresh();
        }
        if let Some(addon) = self.command_list.addon {
            return match addon {
                CommandListAddon::Slack => self.refresh_slack(),
                CommandListAddon::Bookmarks if self.command_list.loading => Task::none(),
                CommandListAddon::Bookmarks => self.load_bookmarks(true),
                CommandListAddon::BrowserHistory if self.command_list.loading => Task::none(),
                CommandListAddon::BrowserHistory => self.load_browser_history(true),
                CommandListAddon::Pass | CommandListAddon::Bitwarden => {
                    let Some(vault) = self.vault_addon() else {
                        return Task::none();
                    };
                    if self.vault.busy() {
                        return Task::none();
                    }
                    self.command_list.begin_refresh();
                    self.start_vault(
                        Op {
                            addon: vault,
                            kind: OpKind::Load,
                        },
                        None,
                    )
                }
                CommandListAddon::Media => self.load_media(true),
                CommandListAddon::Audio => self.load_audio(true),
                CommandListAddon::Kill => {
                    let keep_open = !self.addons.kill.close_on_action(self.close_on_action);
                    self.load_command_list(move || super::kill::load_processes(keep_open), true)
                }
                CommandListAddon::BrowserTabs => self.load_browser_tabs(true),
                CommandListAddon::Bluetooth => self.load_bluetooth(true),
                CommandListAddon::Windows => match (self.compositor)() {
                    Some(compositor) if (self.program_exists)(compositor.program()) => {
                        self.load_windows(compositor, true)
                    }
                    _ => Task::none(),
                },
                CommandListAddon::System => {
                    let query = self.command_list.query.clone();
                    self.handle_command_list_query(CommandListAddon::System, query)
                }
                CommandListAddon::Terminal => Task::none(),
            };
        }
        if self.file_browser.active {
            self.file_browser.current_dir.clear();
            self.file_browser.cached_preview = None;
            self.file_browser.script_previews.clear();
            let query = self.search_query.clone();
            return self.handle_search_changed(query);
        }
        if self.text_snippets.active {
            if let Some(index) = self.text_snippets.source_index {
                let query = self
                    .addons
                    .text_snippets
                    .get(index)
                    .and_then(|config| {
                        self.search_query
                            .trim_start()
                            .strip_prefix(config.keyword.as_str())
                    })
                    .unwrap_or(&self.search_query)
                    .trim_start()
                    .to_string();
                self.text_snippets.file_cache.clear();
                self.text_snippets.clipboard.data = Default::default();
                let mut tasks = Vec::new();
                self.handle_text_snippet_query(index, query, &mut tasks);
                return Task::batch(tasks);
            }
        }
        if let Some(index) = self.script_filter.config_index {
            let query = self.script_filter.query.clone();
            let mut tasks = Vec::new();
            self.handle_script_filter_query(index, query, true, &mut tasks);
            return Task::batch(tasks);
        }
        if self.main_view.active && !self.main_view.sources.is_empty() {
            return self.reload_main_view();
        }
        Task::none()
    }

    pub(super) fn handle_search_changed(&mut self, query: String) -> Task<Message> {
        if self.view.current_modifiers.alt() && !self.history.search_in_progress {
            return Task::none();
        }

        let fido = self.addons.file_browser.enabled
            && self.addons.file_browser.fido_style
            && self.file_browser.active
            && !self.history.search_in_progress
            && !self.view.help_active;
        let mut restarted = false;
        let (query, left_bookmark_folder) = match self.bookmarks_updir(&query) {
            Some((parent, left)) if !self.view.help_active => (parent, Some(left)),
            _ => (query, None),
        };
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
            return self.reveal_reset_selection();
        }

        if query.trim() == "?" && question_mark_opens_help(&self.addons) {
            self.search_query.clear();
            return self.toggle_help();
        }

        let class = classify_query(&query, &self.addons);
        if matches!(class.mode, QueryMode::Standard) {
            if let Some(task) = self.open_main_view_bookmark_keyword(&query) {
                return task;
            }
        }
        let log_query = class.loggable_query(&query, &self.addons);
        self.search_query = query.clone();
        // The input keeps what was typed; everything below sees the expansion.
        let typed = query;
        let query = expand_launcher_keyword(&typed, &self.addons).into_owned();
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
            if self.history.items.get(index).map(|h| h.as_str()) != Some(typed.as_str()) {
                self.history.index = None;
            }
        }

        self.view.refresh_ids();

        let mut tasks = vec![self.reveal_reset_selection()];

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
        let chip = self.active_chip();
        self.keyword_chips = false;
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
        self.drop_suggestions_listed_in_main_view(main_view_query);
        if main_view_query.is_empty() && self.keyword_suggestions.is_empty() {
            self.offer_main_view_plugins(chip);
        }
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
                self.handle_script_filter_query(config_index, query, false, &mut tasks);
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
            QueryMode::Clipboard { query } => {
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
                self.handle_clipboard_query(query, &mut tasks);
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
                if let Some(left) = left_bookmark_folder {
                    self.reselect_bookmark_folder(&left);
                    tasks.push(move_cursor_to_end(self.view.search_input_id.clone()));
                }
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

        let currency_task = if focused {
            self.currency.clear();
            None
        } else {
            self.handle_currency_query(&query, &mut tasks)
        };
        if !focused && matches!(self.query_class.mode, QueryMode::Standard) {
            self.suggest_help_when_empty(&query);
        }

        currency_task.unwrap_or_else(|| Task::batch(tasks))
    }

    /// Offer the help rows matching a query that matches nothing else, so a
    /// feature can be found by what it does rather than by its keyword.
    fn suggest_help_when_empty(&mut self, query: &str) {
        let query = query.trim();
        if query.chars().count() < self.addons.keyword_min_chars()
            || self.total_items() != self.fallback_count()
            || self.main_view.has_loading()
        {
            return;
        }
        self.ensure_help_entries();
        self.keyword_suggestions = help_suggestions(&self.help_entries, query);
        self.keyword_chips = false;
        if !self.keyword_suggestions.is_empty() {
            crate::debug_log!(
                "help: {} rows offered for an unmatched query",
                self.keyword_suggestions.len()
            );
            self.view.refresh_ids();
        }
    }

    /// Put a keyword tip, picked anew on each launch, in the search box.
    pub(super) fn show_search_tip(&mut self) {
        self.ensure_help_entries();
        let tips = search_tips(
            &self.help_entries,
            &self.addons,
            question_mark_opens_help(&self.addons),
        );
        if tips.is_empty() {
            return;
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos() as usize)
            .unwrap_or_default();
        self.view.placeholder = tips[seed % tips.len()].clone();
    }

    fn ensure_help_entries(&mut self) {
        if self.help_entries.is_empty() {
            self.help_entries = help_entries(&self.addons);
            add_cycle_view_shortcut(
                &mut self.help_entries,
                &self.addons,
                !self.main_view.stops.is_empty(),
                self.main_view.chips != crate::MainViewChips::Off,
            );
        }
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
        // Rows are about to be rebuilt: a row at the same index in the new
        // listing is a different file, so a held button must not activate or
        // drag it.
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
                    "No directories here (Ctrl+Tab to show all)".to_string()
                }
                _ => "No files here (Ctrl+Tab for directories)".to_string(),
            });
        }
    }

    fn handle_script_filter_query(
        &mut self,
        config_index: usize,
        query: String,
        force_refresh: bool,
        tasks: &mut Vec<Task<Message>>,
    ) {
        let Some(config) = self.addons.script_filters.get(config_index) else {
            return;
        };
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
        let generation = self.script_filter.generation;
        self.script_filter.loading_name = Some(config.name.clone());
        self.script_filter.action = config.action.clone();
        self.script_filter.secondary_action = config.secondary_action.clone();
        self.script_filter.config_index = Some(config_index);
        self.script_filter.query.clone_from(&query);
        self.script_filter.previews.clear();
        self.script_filter.preview_force_refresh = force_refresh;
        self.script_filter.bump_preview_generation();

        let cache_ttl = config.cache_ttl();
        let mut cached = None;
        let mut stale = false;
        if let Some(ttl) = cache_ttl {
            let key = cache::script_filter_list_key(config, &query);
            if let Some(snapshot) = cache::read_list(&key) {
                if let Some(result) = cache::parse_list(&snapshot, config.icon.clone()) {
                    stale = force_refresh || !snapshot.is_fresh(ttl);
                    cached = Some(result);
                }
            }
        }

        if let Some(result) = cached {
            // Show the snapshot at once; a stale one refreshes underneath.
            self.script_filter.loading = false;
            self.script_filter.results = Some(result);
            self.script_filter.loading_name = None;
            self.script_filter.results_are_cached = true;
            if !stale {
                tasks.push(self.request_script_filter_preview());
                return;
            }
        } else {
            self.script_filter.loading = true;
            self.script_filter.results = None;
            self.script_filter.results_are_cached = false;
        }

        let mut env = config.env.clone();
        if force_refresh {
            env.insert("RAFFI_REFRESH".into(), "1".into());
        }
        let cache_target = cache_ttl.and_then(|ttl| cache::list_target(config, &query, ttl));
        tasks.push(execute_script_filter(
            config.command.clone(),
            config.args.clone(),
            env,
            query,
            generation,
            config.icon.clone(),
            cache_target,
        ));
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
        if addon == CommandListAddon::Slack {
            return self.handle_slack_query(query);
        }
        if addon == CommandListAddon::Bookmarks {
            return self.handle_bookmarks_query(query);
        }
        if addon == CommandListAddon::BrowserHistory {
            return self.handle_browser_history_query(query);
        }
        let entered = self.command_list.enter(addon);
        self.command_list.set_query(query);
        match addon {
            CommandListAddon::System => {
                let session_id = std::env::var("XDG_SESSION_ID").ok();
                let keep_open = !self.should_close(self.addons.system.close_on_action);
                let items = system_entries(
                    &self.addons.system,
                    (self.compositor)(),
                    session_id.as_deref().filter(|id| !id.is_empty()),
                    self.program_exists,
                )
                .iter()
                .map(|entry| entry.to_item_with(keep_open))
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
                self.load_windows(compositor, false)
            }
            CommandListAddon::BrowserTabs if !entered => Task::none(),
            CommandListAddon::BrowserTabs => {
                let load = self.load_browser_tabs(false);
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
                let keep_open = !self.addons.kill.close_on_action(self.close_on_action);
                self.load_command_list(move || super::kill::load_processes(keep_open), false)
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
                let keep_open = !self.should_close(self.addons.terminal.close_on_action);
                self.command_list.query_row = Some(std::sync::Arc::new(move |query: &str| {
                    let mut item = super::terminal::run_item_with(
                        query,
                        &prefix,
                        &shell,
                        hold,
                        tab.as_ref(),
                        keep_open,
                    );
                    item.icon = icon.clone();
                    Some(item)
                }));
                self.set_command_list_items(Vec::new());
                Task::none()
            }
            // Handled above.
            CommandListAddon::Slack
            | CommandListAddon::Bookmarks
            | CommandListAddon::BrowserHistory => Task::none(),
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
        let addon_close = match op.addon {
            VaultAddon::Pass => self.addons.pass.close_on_action,
            VaultAddon::Bitwarden => self.addons.bitwarden.close_on_action,
        };
        let should_close = match &report.outcome {
            Outcome::Ready(job) => !job.can_run_kept_open() || self.should_close(addon_close),
            _ => true,
        };
        if let Some(notice) = &report.notice {
            if ready && should_close {
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
                self.save_query_to_history();
                if let Some(identity) = op.identity() {
                    record_mru_use(&mut self.vault.usage, identity.clone());
                    save_mru_map(self.vault.usage_path.as_deref(), &self.vault.usage);
                    if op.addon == VaultAddon::Pass {
                        self.record_recent(RecentPick::new("pass".to_string(), identity));
                    }
                }
                if !should_close {
                    crate::debug_log!("vault: running {} while open", op.log_name());
                    let entry = match &op.kind {
                        OpKind::Entry(_, name) | OpKind::Copy(name) => name.clone(),
                        _ => String::new(),
                    };
                    let copy_op = Op {
                        addon: op.addon,
                        kind: OpKind::Copy(entry),
                    };
                    self.vault.stop_search();
                    let (generation, cancel) = self.vault.begin(copy_op);
                    self.sync_vault();
                    return Task::perform(
                        async move {
                            let (sender, receiver) = iced::futures::channel::oneshot::channel();
                            std::thread::spawn(move || {
                                let runner = SystemRunner { cancel };
                                let outcome = match job.run_with(&runner) {
                                    Ok(()) => Outcome::Done,
                                    Err(error) => Outcome::Failed(error),
                                };
                                let _ = sender.send(Report {
                                    outcome,
                                    notice: None,
                                });
                            });
                            receiver.await.unwrap_or_else(|_| {
                                Report::failed(SecretError::Tool(
                                    "stopped unexpectedly".to_string(),
                                ))
                            })
                        },
                        move |report| Message::VaultDone(generation, report),
                    );
                }
                crate::debug_log!("vault: running {} after close", op.log_name());
                if let Ok(mut slot) = self.deferred_action.lock() {
                    *slot = Some(super::actions::DeferredAction::Secret(job));
                }
                iced::exit()
            }
            Outcome::Done if matches!(op.kind, OpKind::Copy(_)) => Task::none(),
            Outcome::Done if op.kind == OpKind::Lock => {
                if self.should_close(addon_close) {
                    iced::exit()
                } else {
                    Task::none()
                }
            }
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
        self.keyword_rows() + self.script_filter_offset() + snippets + emoji + clipboard
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
        let keep_open = !self.addons.bluetooth.close_on_action(self.close_on_action);
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

    fn load_browser_tabs(&mut self, refresh: bool) -> Task<Message> {
        let view = self.command_list.tabs_view;
        let focus_keep_open = !self.should_close(self.addons.browser_tabs.close_on_action);
        let close_keep_open = self.addons.browser_tabs.close_on_action != Some(true);
        self.load_command_list(
            move || super::browser_tabs::load_tabs(view, focus_keep_open, close_keep_open),
            refresh,
        )
    }

    /// Start the launcher at this config index, closing Raffi unless the
    /// launcher keeps it open.
    fn launch_config(&mut self, index: usize) -> Task<Message> {
        let Some(config) = self.configs.get(index).cloned() else {
            return Task::none();
        };
        let description = config
            .description
            .clone()
            .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
        let close = self.should_close(config.close_on_action);
        record_mru_use(&mut self.mru_map, description.clone());
        save_mru_map(get_mru_cache_path().ok().as_deref(), &self.mru_map);
        self.save_query_to_history();
        if !close && !self.print_only {
            if let Err(e) = crate::spawn_launcher_command(
                &config,
                &self.view.default_script_shell,
                self.view.launch_via,
            ) {
                eprintln!("Error executing command: {e:#}");
            }
            return Task::none();
        }
        if let Ok(mut selected) = self.selected_item.lock() {
            *selected = Some(description);
        }
        iced::exit()
    }

    /// Focus the launcher's open window found on Enter, or start it when
    /// there is none.
    fn handle_launcher_raise_checked(
        &mut self,
        index: usize,
        focus: Option<super::actions::DeferredAction>,
    ) -> Task<Message> {
        let Some(focus) = focus else {
            return self.launch_config(index);
        };
        let Some(config) = self.configs.get(index) else {
            return Task::none();
        };
        let description = config
            .description
            .clone()
            .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
        let close = self.should_close(config.close_on_action);
        crate::debug_log!("raise: focusing the open {description:?} window");
        record_mru_use(&mut self.mru_map, description);
        save_mru_map(get_mru_cache_path().ok().as_deref(), &self.mru_map);
        self.save_query_to_history();
        if !close {
            return Task::future(async move {
                if let Some(Err(error)) = super::browser_data::off_thread(move || focus.run()).await
                {
                    crate::debug_log!("raise: {error}");
                }
            })
            .discard();
        }
        if let Ok(mut slot) = self.deferred_action.lock() {
            *slot = Some(focus);
        }
        iced::exit()
    }

    /// Reload the window switcher after the compositor reported a change,
    /// unless a load, an armed confirmation or an action would be disturbed.
    fn handle_windows_changed(&mut self) -> Task<Message> {
        let list = &self.command_list;
        if list.addon != Some(CommandListAddon::Windows)
            || list.loading
            || list.refreshing
            || list.armed.is_some()
            || list.closing.is_some()
            || list.overlay.is_some()
        {
            return Task::none();
        }
        match (self.compositor)() {
            Some(compositor) => self.load_windows(compositor, true),
            None => Task::none(),
        }
    }

    /// Load the windows or the workspaces, whichever the switcher shows.
    fn load_windows(
        &mut self,
        compositor: super::windows::Compositor,
        refresh: bool,
    ) -> Task<Message> {
        let view = self.command_list.windows_view;
        let keep_open = !self.should_close(self.addons.windows.close_on_action);
        self.load_command_list(move || compositor.load_view(view, keep_open), refresh)
    }

    fn load_audio(&mut self, refresh: bool) -> Task<Message> {
        let options = super::audio::AudioOptions::of(&self.addons.audio, self.close_on_action);
        self.command_list.query_row = Some(self.audio_query_row(true));
        self.load_command_list(move || Ok(super::audio::load_devices(options)), refresh)
    }

    /// The "Set output volume" row builder; `bare` accepts a lone number.
    pub(super) fn audio_query_row(&self, bare: bool) -> super::command_list::QueryRow {
        let options = super::audio::AudioOptions::of(&self.addons.audio, self.close_on_action);
        let icon = self.resolve_icon_name(super::audio::SET_VOLUME_ICON);
        std::sync::Arc::new(move |query: &str| {
            let mut item = super::audio::set_volume_item(query, bare, options)?;
            item.icon = icon.clone();
            Some(item)
        })
    }

    fn load_media(&mut self, refresh: bool) -> Task<Message> {
        let keep_open = !self.addons.media.close_on_action(self.close_on_action);
        self.load_command_list(move || super::media::load_players(keep_open), refresh)
    }

    /// Run `load` off the UI thread; its result is dropped if the list has
    /// moved on by then. A refresh keeps the current rows until it lands.
    pub(super) fn load_command_list(
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
        if self.command_list.addon == Some(CommandListAddon::Slack) {
            self.sync_slack_account();
        }
        if !self.command_list.is_current(generation) {
            return Task::none();
        }
        let selected = self
            .command_list
            .item(self.selected_index)
            .map(|item| item.identity.clone());
        let refreshing = self.command_list.refreshing;
        match result {
            Ok(items) => self.set_command_list_items(items),
            Err(error) => {
                crate::debug_log!("command list: load failed: {error}");
                self.command_list.set_notice(error);
            }
        }
        self.selected_index = match selected.and_then(|id| self.command_list.position(&id)) {
            Some(row) => row,
            // The selected row went away, e.g. a closed tab: select what
            // now sits there, so the next one can be closed right away.
            None if refreshing => self
                .selected_index
                .min(self.command_list.row_count().saturating_sub(1)),
            None => 0,
        };
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
        if self.command_list.closing.as_ref() == Some(&identity) {
            self.command_list.closing = None;
        }
        let main_view = self.invalidate_main_view(addon);
        if self.command_list.addon != Some(addon) || self.command_list.session != session {
            if let Err(error) = result {
                if self.main_view.active {
                    return self.show_main_view_action_error(addon, &identity, error);
                }
            }
            return main_view;
        }
        let task = match result {
            Ok(()) => match addon {
                CommandListAddon::Bookmarks | CommandListAddon::BrowserHistory => Task::none(),
                CommandListAddon::Media => self.load_media(true),
                CommandListAddon::Audio => self.load_audio(true),
                CommandListAddon::Kill => {
                    let keep_open = !self.addons.kill.close_on_action(self.close_on_action);
                    self.load_command_list(move || super::kill::load_processes(keep_open), true)
                }
                CommandListAddon::BrowserTabs => self.load_browser_tabs(true),
                CommandListAddon::Bluetooth => self.load_bluetooth(true),
                CommandListAddon::Slack => self.load_slack(true),
                CommandListAddon::Windows => match (self.compositor)() {
                    Some(compositor) => self.load_windows(compositor, true),
                    None => Task::none(),
                },
                CommandListAddon::System
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
                        let keep_open = !self.addons.kill.close_on_action(self.close_on_action);
                        self.load_command_list(move || super::kill::load_processes(keep_open), true)
                    }
                    CommandListAddon::BrowserTabs => self.load_browser_tabs(true),
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
    pub(super) fn set_command_list_front(&mut self, mut item: super::command_list::CommandItem) {
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
        let action = if self.command_list.closing.as_ref() == Some(&item.identity) {
            &item.close
        } else {
            &item.primary
        };
        action.as_ref()?.progress.as_deref()
    }

    pub(super) fn set_command_list_items(
        &mut self,
        mut items: Vec<super::command_list::CommandItem>,
    ) {
        if self.command_list.addon == Some(CommandListAddon::Slack) {
            self.apply_slack_keep_open(&mut items);
        }
        for item in &mut items {
            if item.icon.is_none() {
                item.icon = item
                    .icon_names
                    .iter()
                    .find_map(|name| self.resolve_icon_name(name));
            }
            if let Some(group) = &mut item.group {
                group.icon = group
                    .icon_name
                    .as_deref()
                    .and_then(|name| self.resolve_icon_name(name));
            }
        }
        if self.command_list.addon == Some(CommandListAddon::Slack) {
            self.apply_slack_images(&mut items);
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

    /// Scrolls the list just enough to show the selected row. Rows differ in
    /// height (wrapped titles, headings), so the row is measured rather than
    /// placed by its index.
    pub(super) fn snap_to_selection(&self) -> Task<Message> {
        let total = self.total_items();
        let relative = if total > 1 {
            self.selected_index as f32 / (total - 1) as f32
        } else {
            0.0
        };
        // The first and last rows go to the ends, so chips and headings
        // above the first row stay in view.
        let edge =
            (self.selected_index == 0 || self.selected_index + 1 >= total).then_some(relative);
        iced::advanced::widget::operate::<()>(RevealSelection {
            scrollable: self.view.scrollable_id.clone(),
            row: self.view.selected_row_id.clone(),
            relative,
            edge,
            viewport: None,
            row_bounds: None,
        })
        .discard()
    }

    /// Scrolls back to a selection reset by a new list. The list keeps its
    /// offset when its id is refreshed, so one scrolled down would hide it.
    fn reveal_reset_selection(&self) -> Task<Message> {
        if self.view.list_scrolled {
            self.snap_to_selection()
        } else {
            Task::none()
        }
    }

    fn activate_index(&mut self, index: usize) -> Task<Message> {
        if self.view.help_active {
            return self.update(Message::HelpEntrySelected(index));
        }
        if self.command_list.addon == Some(CommandListAddon::BrowserHistory)
            && self.flush_browser_history_search()
        {
            return self.activate_index(self.command_list_offset());
        }

        let mut current_index = 0;

        if index < self.keyword_rows() {
            return self.update(Message::KeywordSuggestionSelected(index));
        }
        current_index += self.keyword_rows();

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
            let config = self.configs[real_config_index].clone();
            let description = config
                .description
                .clone()
                .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
            if let Some(alias) = &config.alias {
                let text = match &config.keyword {
                    Some(keyword) => format!("{keyword} "),
                    None => alias.clone(),
                };
                record_mru_use(&mut self.mru_map, description);
                save_mru_map(get_mru_cache_path().ok().as_deref(), &self.mru_map);
                return self.type_alias(text);
            }
            if !self.print_only && !self.view.current_modifiers.control() {
                if let Some(compositor) = self.view.raise_via {
                    let classes = crate::raise_classes(&config, self.view.raise_running);
                    if !classes.is_empty() {
                        crate::debug_log!("raise: looking for a {classes:?} window");
                        return Task::perform(
                            super::browser_data::off_thread(move || {
                                super::windows::find_app_window(compositor, &classes)
                            }),
                            move |found| {
                                let focus = match found {
                                    Some(Ok(focus)) => focus,
                                    Some(Err(error)) => {
                                        crate::debug_log!("raise: {error}");
                                        None
                                    }
                                    None => None,
                                };
                                Message::LauncherRaiseChecked(real_config_index, focus)
                            },
                        );
                    }
                }
            }
            return self.launch_config(real_config_index);
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
                let addon_close = if self.devtools.active {
                    self.addons.devtools.close_on_action
                } else {
                    match self.query_class.inline_provider {
                        Some(InlineProvider::Calculator) => self.addons.calculator.close_on_action,
                        Some(InlineProvider::Currency) => self.addons.currency.close_on_action,
                        Some(InlineProvider::Units) => self.addons.units.close_on_action,
                        Some(InlineProvider::Color) => self.addons.color.close_on_action,
                        Some(InlineProvider::Time) => self.addons.world_clock.close_on_action,
                        Some(InlineProvider::Uuid | InlineProvider::Password) => {
                            self.addons.devtools.close_on_action
                        }
                        None => None,
                    }
                };
                self.finish_or_keep_open(addon_close)
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
        match result {
            Ok(result) => {
                self.script_filter.results = Some(result);
                self.script_filter.results_are_cached = false;
                // Indices from the old rows mean nothing once new rows land.
                self.script_filter.previews.clear();
                self.script_filter.bump_preview_generation();
            }
            Err(error) => {
                if self.script_filter.results_are_cached {
                    // A failed revalidation keeps the rows the user can still use.
                    crate::debug_log!(
                        "script_filter: background refresh failed, keeping cached rows: {error}"
                    );
                } else {
                    self.script_filter.results = None;
                }
            }
        }
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
        let index = self.selected_index.checked_sub(self.keyword_rows())?;
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
        if let Some(ttl) = config.cache_ttl() {
            // A timer wake for a preview already on screen is a live refresh,
            // and Ctrl+R asks for live data too: neither may reuse a snapshot.
            let visible = self
                .script_filter
                .previews
                .get(&index)
                .and_then(|dynamic| dynamic.preview.as_ref())
                .is_some();
            if !visible && !self.script_filter.preview_force_refresh {
                let key = cache::script_filter_preview_key(&request);
                if let Some(snapshot) = cache::read_preview(&key) {
                    if let Ok(Some(preview)) = parse_preview_response(snapshot.payload.as_bytes()) {
                        self.script_filter.preview_supported.insert(config_index);
                        self.script_filter.preview_probing.remove(&config_index);
                        let entry = self.script_filter.previews.entry(index).or_default();
                        let refresh = preview.refresh;
                        entry.preview = Some(preview);
                        entry.error = None;
                        if snapshot.is_fresh(ttl) {
                            // Revalidate after the script's own refresh interval,
                            // or when the snapshot's freshness window ends.
                            let delay =
                                refresh.unwrap_or_else(|| ttl.saturating_sub(snapshot.age()));
                            return schedule_script_filter_preview(
                                delay,
                                generation,
                                self.script_filter.preview_live_generation.clone(),
                                index,
                            );
                        }
                    }
                }
            }
            request.cache = cache::preview_target(&request, ttl);
        }
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
        let mut addon_close = None;
        let mut force_close = false;
        if let Some(result) = &self.script_filter.results {
            if let Some(item) = result.items.get(index) {
                if !item.valid {
                    return Task::none();
                }
                if let Some(config) = self
                    .script_filter
                    .config_index
                    .and_then(|index| self.addons.script_filters.get(index))
                {
                    addon_close = config.close_on_action;
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
                let act = action.unwrap_or("copy");
                force_close = act == "insert";
                execute_action_with_env(act, value, &item.variables);
            }
        }
        if let Some(pick) = pick {
            self.record_recent(pick);
        }
        if force_close {
            self.save_query_to_history();
            iced::exit()
        } else {
            self.finish_or_keep_open(addon_close)
        }
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
                let query = match &self.query_class.mode {
                    QueryMode::TextSnippet { query, .. } => query.clone(),
                    _ => String::new(),
                };
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
        let addon_close = self
            .text_snippets
            .source_index
            .and_then(|index| self.addons.text_snippets.get(index))
            .and_then(|config| config.close_on_action);
        let mut force_close = false;
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
                    restore_main_view: None,
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
            force_close = action == "insert";
            execute_action(&action, &snippet.value);
        }
        if force_close {
            self.save_query_to_history();
            iced::exit()
        } else {
            self.finish_or_keep_open(addon_close)
        }
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
                if pending.action == "insert" {
                    self.save_query_to_history();
                    iced::exit()
                } else {
                    let addon_close = self
                        .text_snippets
                        .source_index
                        .and_then(|index| self.addons.text_snippets.get(index))
                        .and_then(|config| config.close_on_action);
                    if let Some(snapshot) = pending.restore_main_view {
                        if !self.should_close(addon_close) {
                            self.save_query_to_history();
                            return self.restore_main_view_context(snapshot);
                        }
                    }
                    self.finish_or_keep_open(addon_close)
                }
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
            restore_main_view: None,
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
        let addon_close = match &self.query_class.mode {
            QueryMode::WebSearch { config_index, .. } => self
                .addons
                .web_searches
                .get(*config_index)
                .and_then(|c| c.close_on_action),
            _ => None,
        };
        self.finish_or_keep_open(addon_close)
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
        super::release_renderer_override();
        crate::debug_log!(
            "startup: window created at {}ms",
            crate::debug::elapsed_ms()
        );
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
                Task::none()
            }
            super::dnd::DragEvent::Cancelled => {
                self.file_browser.dragging = false;
                Task::none()
            }
            super::dnd::DragEvent::Dropped => {
                self.file_browser.dragging = false;
                self.finish_or_keep_open(self.addons.file_browser.close_on_action)
            }
        }
    }

    fn handle_file_browser_selected(&mut self, index: usize) -> Task<Message> {
        let mut force_close = false;
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
            force_close = action == "insert";
            execute_file_action(action, &entry.full_path);
        }

        if force_close {
            self.save_query_to_history();
            iced::exit()
        } else {
            self.finish_or_keep_open(self.addons.file_browser.close_on_action)
        }
    }

    /// Ctrl+Tab, backwards with Shift: switch to the next view of what is
    /// showing. Tab completion where nothing has views.
    fn handle_cycle_view(&mut self, backwards: bool) -> Task<Message> {
        if self.view.help_active {
            return self.handle_tab_complete();
        }
        if self.clipboard.active {
            return self.handle_clipboard_cycle_view(backwards);
        }
        if self.file_browser.active {
            self.file_browser.show = self.file_browser.show.cycle(backwards);
            return Task::done(Message::SearchChanged(self.search_query.clone()));
        }
        match self.command_list.addon {
            Some(CommandListAddon::Slack) => return self.handle_slack_cycle_view(backwards),
            Some(CommandListAddon::Bookmarks) if self.bookmarks.view_label().is_some() => {
                return self.handle_bookmarks_cycle_view(backwards);
            }
            Some(CommandListAddon::BrowserHistory) => {
                return self.handle_browser_history_cycle_view(backwards);
            }
            Some(CommandListAddon::Audio) => {
                let view = self.command_list.audio_view.cycle(backwards);
                self.command_list.set_audio_view(view);
                return self.handle_search_changed(self.search_query.clone());
            }
            Some(CommandListAddon::BrowserTabs) => {
                self.command_list.tabs_view = self.command_list.tabs_view.cycle();
                return self.load_browser_tabs(true);
            }
            Some(CommandListAddon::Windows) => {
                if let Some(compositor) = (self.compositor)() {
                    self.command_list.windows_view = self.command_list.windows_view.cycle();
                    return self.load_windows(compositor, false);
                }
            }
            _ => {}
        }
        if self.main_view.active && !self.main_view.stops.is_empty() {
            self.main_view.cycle(backwards);
            return self.handle_search_changed(self.search_query.clone());
        }
        self.handle_tab_complete()
    }

    /// Name of the view Ctrl+Tab switched to, for the footer; None where
    /// Ctrl+Tab has no views.
    pub(super) fn cycle_view_label(&self) -> Option<String> {
        if self.view.help_active {
            return None;
        }
        if self.clipboard.active {
            return Some(self.clipboard.view.label().to_string());
        }
        if self.file_browser.active {
            return Some(self.file_browser.show.title().to_string());
        }
        match self.command_list.addon {
            Some(CommandListAddon::Slack) => {
                return self
                    .slack
                    .view
                    .as_ref()
                    .map(|view| view.label().to_string());
            }
            Some(CommandListAddon::Audio) => {
                return Some(self.command_list.audio_view.label().to_string());
            }
            Some(CommandListAddon::BrowserTabs) => {
                return Some(self.command_list.tabs_view.label().to_string());
            }
            Some(CommandListAddon::Windows) if (self.compositor)().is_some() => {
                return Some(self.command_list.windows_view.label().to_string());
            }
            Some(CommandListAddon::Bookmarks) if self.bookmarks.prompt.is_some() => return None,
            Some(CommandListAddon::Bookmarks) => return self.bookmarks.view_label(),
            Some(CommandListAddon::BrowserHistory) => {
                return Some(self.browser_history.view_label());
            }
            _ => {}
        }
        (self.main_view.active && !self.main_view.stops.is_empty())
            .then(|| self.main_view_focus_label())
    }

    /// Ctrl+Tab in Slack: rewrite the view word after the keyword, going
    /// Home, Unread, Threads, Activity, People, Actions, keeping the rest of the
    /// text. The typed keyword stays when it still routes there; a launcher
    /// alias carrying its own view or text falls back to the Slack trigger. Nothing changes when a
    /// launcher keyword shadowing the trigger makes the view unreachable.
    fn handle_slack_cycle_view(&mut self, backwards: bool) -> Task<Message> {
        let slack_query = |input: &str| match route_query(
            &expand_launcher_keyword(input, &self.addons),
            &self.addons,
        ) {
            QueryMode::CommandList {
                addon: CommandListAddon::Slack,
                query,
            } => Some(query),
            _ => None,
        };
        let Some(query) = slack_query(&self.search_query) else {
            return Task::none();
        };
        let cycled = super::slack::cycle_view(&query, backwards);
        let typed = self.search_query.trim_end();
        let heads = [
            typed.strip_suffix(query.as_str()).map(str::trim_end),
            Some(self.addons.slack.trigger()),
        ];
        let Some(new_query) = heads
            .into_iter()
            .flatten()
            .map(|head| format!("{head} {cycled}"))
            .find(|candidate| slack_query(candidate).as_deref() == Some(cycled.trim_end()))
        else {
            return Task::none();
        };
        self.search_query = new_query.clone();
        let input_id = self.view.search_input_id.clone();
        Task::done(Message::SearchChanged(new_query)).chain(move_cursor_to_end(input_id))
    }

    fn handle_tab_complete(&mut self) -> Task<Message> {
        if self.view.help_active {
            return self.update(Message::HelpEntrySelected(self.selected_index));
        }

        if self.selected_index < self.keyword_rows() {
            return self.update(Message::KeywordSuggestionSelected(self.selected_index));
        }

        if let Some(task) = self
            .selected_index
            .checked_sub(self.command_list_offset())
            .and_then(|row| {
                self.open_bookmark_folder(row)
                    .or_else(|| self.open_bookmark_search(row))
            })
        {
            return task;
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
        self.finish_or_keep_open(self.addons.file_browser.close_on_action)
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
        self.finish_or_keep_open(self.addons.file_browser.close_on_action)
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
                let emoji_query = match &self.query_class.mode {
                    QueryMode::Emoji { query } => query.clone(),
                    _ => String::new(),
                };
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
        let mut force_close = false;
        if let Some(&emoji_index) = self.emoji.filtered.get(index) {
            if let Some(entry) = self.emoji.data.get(emoji_index) {
                let action = if self.view.current_modifiers.control() {
                    self.emoji.secondary_action.as_deref().unwrap_or("insert")
                } else {
                    self.emoji.action.as_deref().unwrap_or("copy")
                };
                force_close = action == "insert";
                execute_action(action, &entry.value);
            }
        }
        if force_close {
            self.save_query_to_history();
            iced::exit()
        } else {
            self.finish_or_keep_open(self.addons.emoji.close_on_action)
        }
    }

    /// Runs the item's action after Raffi closes. Actions needing
    /// confirmation run on a second Enter on the same target with the same
    /// modifiers.
    pub(super) fn handle_command_list_selected(&mut self, row: usize) -> Task<Message> {
        if self.command_list.addon == Some(CommandListAddon::BrowserHistory)
            && self.flush_browser_history_search()
        {
            return Task::none();
        }
        let Some(addon) = self.command_list.addon else {
            return Task::none();
        };
        if let Some(vault) = VaultAddon::of(addon) {
            return self.select_vault_row(vault, row, None);
        }
        if let Some(task) = self
            .open_bookmark_folder(row)
            .or_else(|| self.open_bookmark_search(row))
        {
            return task;
        }
        match self.command_list.select(row, self.view.current_modifiers) {
            Selection::Ignore => Task::none(),
            Selection::Armed => {
                crate::debug_log!("{}: arming confirmation", addon.name());
                Task::none()
            }
            Selection::Run(action, identity, keep_open) => {
                if let DeferredAction::Slack(super::slack::SlackAction::Fill(text)) = &action {
                    return self.fill_slack_query(text);
                }
                self.run_command_list_action(addon, identity, action, keep_open)
            }
        }
    }

    /// Shared tail of the command-list run paths: mark the run in flight,
    /// record the picks, then either run the action in the background while
    /// staying open or defer it until Raffi has closed.
    pub(super) fn run_command_list_action(
        &mut self,
        addon: CommandListAddon,
        identity: String,
        action: DeferredAction,
        keep_open: bool,
    ) -> Task<Message> {
        if !self.in_flight.begin(addon.name(), &identity) {
            return Task::none();
        }
        self.record_command_list_pick(addon, &identity);
        if addon == CommandListAddon::Slack {
            self.record_slack_pick(&identity);
        }
        if keep_open {
            crate::debug_log!("{}: running {identity}", addon.name());
            self.save_query_to_history();
            return self.run_kept_open_task(addon, identity, action);
        }
        crate::debug_log!("{}: running {identity} after close", addon.name());
        if let Ok(mut slot) = self.deferred_action.lock() {
            *slot = Some(action);
        }
        self.save_query_to_history();
        iced::exit()
    }

    /// Ctrl+D on a command list row: run its close action, such as closing
    /// a window, with Raffi kept open.
    pub(super) fn handle_command_list_close(&mut self) -> Task<Message> {
        if self.view.help_active || !self.command_list.is_active() {
            return Task::none();
        }
        let Some(addon) = self.command_list.addon else {
            return Task::none();
        };
        let Some((identity, action)) = self
            .selected_index
            .checked_sub(self.command_list_offset())
            .and_then(|row| self.command_list.item(row))
            .filter(|item| item.unavailable.is_none())
            .and_then(|item| Some((item.identity.clone(), item.close.as_ref()?.action.clone())))
        else {
            return Task::none();
        };
        if !self.in_flight.begin(addon.name(), &identity) {
            return Task::none();
        }
        crate::debug_log!("{}: closing {identity}", addon.name());
        self.command_list.armed = None;
        self.command_list.closing = Some(identity.clone());
        self.run_kept_open_task(addon, identity, action)
    }

    /// Run `action` off the UI thread while Raffi stays open, reporting back
    /// with [`Message::CommandListActionDone`].
    fn run_kept_open_task(
        &mut self,
        addon: CommandListAddon,
        identity: String,
        action: DeferredAction,
    ) -> Task<Message> {
        let session = self.command_list.session;
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let result = if matches!(
                        addon,
                        CommandListAddon::Terminal
                            | CommandListAddon::System
                            | CommandListAddon::Bookmarks
                            | CommandListAddon::BrowserHistory
                    ) {
                        action.run()
                    } else {
                        super::command_list::run_kept_open(&action)
                    };
                    let _ = sender.send(result);
                });
                let result = receiver
                    .await
                    .unwrap_or_else(|_| Err("action stopped unexpectedly".to_string()));
                (identity, result)
            },
            move |(identity, result)| {
                Message::CommandListActionDone(addon, session, identity, result)
            },
        )
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
            || (addon == CommandListAddon::Windows
                && self.command_list.windows_view != super::windows::View::Windows)
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
        let mut offset = self.keyword_rows();
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

    /// Whether the query is the start of a suggested keyword. Rows offered
    /// for what a feature does leave the fallbacks listed.
    fn typing_keyword(&self) -> bool {
        let input = self.search_query.trim().to_lowercase();
        self.keyword_suggestions
            .iter()
            .any(|suggestion| suggestion.keyword.to_lowercase().starts_with(&input))
    }

    pub(super) fn fallback_count(&self) -> usize {
        if !self.view.help_active
            && !self.search_query.trim().is_empty()
            && !self.is_in_addon_mode()
            && !self.typing_keyword()
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
                        close_on_action: terminal.close_on_action,
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
                            close_on_action: ws.close_on_action,
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
                            close_on_action: sf.close_on_action,
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
                    return self.finish_or_keep_open(fallback.close_on_action);
                }
                FallbackAction::ScriptFilter {
                    command,
                    args,
                    action,
                    ..
                } => {
                    let act = action.as_deref().unwrap_or("copy");
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
                                    execute_action(act, value);
                                }
                            }
                        }
                    }
                    if act == "insert" {
                        self.save_query_to_history();
                        return iced::exit();
                    }
                    return self.finish_or_keep_open(fallback.close_on_action);
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
                    if !self.should_close(fallback.close_on_action) {
                        std::thread::spawn(move || {
                            if let Err(error) = action.run() {
                                eprintln!("raffi: {error}");
                            }
                        });
                        self.save_query_to_history();
                        return Task::none();
                    }
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
        self.ensure_help_entries();
        self.filtered_configs.clear();
        self.keyword_suggestions.clear();
        self.keyword_chips = false;
        self.inline_result = None;
        self.invalidate_inline_copy();
        self.script_filter.clear();
        self.text_snippets.clear();
        self.emoji.clear();
        self.clipboard.clear();
        self.command_list.clear();
        self.bookmarks.prompt = None;
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
        self.reveal_reset_selection()
    }

    fn close_help(&mut self) -> Task<Message> {
        let query = self
            .help_saved_query
            .take()
            .expect("help saves the search query when it opens");
        self.view.help_active = false;
        self.help_filtered.clear();
        crate::debug_log!("help: closed");
        // Escape also unfocuses the text input, which would swallow typing.
        let input_id = self.view.search_input_id.clone();
        self.handle_search_changed(query)
            .chain(focus(input_id.clone()))
            .chain(move_cursor_to_end(input_id))
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

    /// Rows the keyword offers take: one for the chips row, else one each.
    pub(super) fn keyword_rows(&self) -> usize {
        if self.keyword_chips {
            self.keyword_suggestions.len().min(1)
        } else {
            self.keyword_suggestions.len()
        }
    }

    /// Keyword offered by list row `row`.
    pub(super) fn keyword_at(&self, row: usize) -> Option<&KeywordSuggestion> {
        if !self.keyword_chips {
            self.keyword_suggestions.get(row)
        } else if row == 0 {
            self.keyword_suggestions.get(self.chip_index)
        } else {
            None
        }
    }

    /// Keyword of the chip the chips row selects.
    pub(super) fn active_chip(&self) -> Option<String> {
        self.keyword_chips
            .then(|| self.keyword_at(0))
            .flatten()
            .map(|suggestion| suggestion.keyword.clone())
    }

    fn handle_chip_move(&mut self, backwards: bool) -> Task<Message> {
        let count = self.keyword_suggestions.len();
        if !self.keyword_chips || self.selected_index != 0 || count < 2 {
            return Task::none();
        }
        self.chip_index = if backwards {
            (self.chip_index + count - 1) % count
        } else {
            (self.chip_index + 1) % count
        };
        self.view.preview_scrollable_id = super::state::ScrollableId::unique();
        Task::none()
    }

    fn handle_keyword_suggestion_selected(&mut self, index: usize) -> Task<Message> {
        let Some(suggestion) = self.keyword_at(index) else {
            return Task::none();
        };

        // A path such as `~/` is already complete; a space would end it.
        let new_query = if suggestion.keyword.ends_with('/') {
            suggestion.keyword.clone()
        } else {
            format!("{} ", suggestion.keyword)
        };
        crate::debug_log!("keyword suggestion: completing to {new_query:?}");
        let input_id = self.view.search_input_id.clone();
        self.update(Message::SearchChanged(new_query))
            .chain(move_cursor_to_end(input_id))
    }

    /// Type a picked alias launcher's keyword or alias into the input; it
    /// never runs anything by itself.
    fn type_alias(&mut self, text: String) -> Task<Message> {
        crate::debug_log!("alias: typing {:?}", loggable_query(&text, &self.addons));
        // Alt would make the search change a no-op.
        let modifiers = std::mem::take(&mut self.view.current_modifiers);
        let task = self.update(Message::SearchChanged(text));
        self.view.current_modifiers = modifiers;
        task.chain(move_cursor_to_end(self.view.search_input_id.clone()))
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

/// Measures the selected row and the list viewport, then scrolls the list
/// only if the row is not fully visible.
struct RevealSelection {
    scrollable: super::state::ScrollableId,
    row: super::state::ContainerId,
    /// Proportional position, used when the row is not tagged.
    relative: f32,
    /// Set for the first and last rows, which snap to the list ends.
    edge: Option<f32>,
    /// Viewport bounds, content bounds and current scroll offset.
    viewport: Option<(iced::Rectangle, iced::Rectangle, iced::Vector)>,
    row_bounds: Option<iced::Rectangle>,
}

impl<T: 'static> iced::advanced::widget::Operation<T> for RevealSelection {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<T>)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&iced::advanced::widget::Id>, bounds: iced::Rectangle) {
        if id == Some(&self.row) {
            self.row_bounds = Some(bounds);
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        bounds: iced::Rectangle,
        content_bounds: iced::Rectangle,
        translation: iced::Vector,
        _state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&self.scrollable) {
            self.viewport = Some((bounds, content_bounds, translation));
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<T> {
        use iced::advanced::widget::operation::{scrollable as ops, Outcome};
        let target = self.scrollable.clone();
        let snap = |y: f32| -> Outcome<T> {
            Outcome::Chain(Box::new(ops::snap_to(
                target.clone(),
                scrollable::RelativeOffset {
                    x: None,
                    y: Some(y),
                },
            )))
        };
        let (Some((viewport, content, translation)), Some(row)) = (self.viewport, self.row_bounds)
        else {
            return snap(self.edge.unwrap_or(self.relative));
        };
        // Go to the list end only if the row stays fully visible there; a
        // tall heading or error next to it could otherwise hide it.
        let offset = match self.edge {
            Some(edge) => edge * (content.height - viewport.height).max(0.0),
            None => translation.y,
        };
        match reveal_offset(row.y - content.y, row.height, offset, viewport.height)
            .or(self.edge.map(|_| offset))
        {
            Some(y) => Outcome::Chain(Box::new(ops::scroll_to(
                target,
                AbsoluteOffset {
                    x: None,
                    y: Some(y),
                },
            ))),
            None => Outcome::None,
        }
    }
}

/// Scroll offset that shows a row at `top` (from the top of the content) of
/// `height` in a viewport of `viewport` height scrolled to `offset`, or
/// `None` when the row is already fully visible. A row taller than the
/// viewport shows its top.
pub(super) fn reveal_offset(top: f32, height: f32, offset: f32, viewport: f32) -> Option<f32> {
    let bottom = top + height;
    if top < offset {
        Some(top)
    } else if bottom > offset + viewport {
        Some((bottom - viewport).min(top))
    } else {
        None
    }
}
