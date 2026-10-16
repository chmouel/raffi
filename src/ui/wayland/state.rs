use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fuzzy_matcher::skim::SkimMatcherV2;
use iced::widget::Id;

use super::help::HelpEntry;
use super::preview::InspectedFile;
use super::theme::{PreviewStyle, ThemeColors};
pub(crate) use super::types::{
    CachedRate, CalculatorResult, ClipboardData, CurrencyConversion, CurrencyConversionRequest,
    CurrencyResult, EmojiEntry, FileBrowserEntry, Message, MultiCurrencyRequest,
    MultiCurrencyResult, ScriptFilterResult, WebSearchActiveState,
};
use crate::{AddonsConfig, TextSnippet};

pub(super) type ContainerId = Id;
pub(super) type ScrollableId = Id;
pub(super) type TextInputId = Id;
pub(super) type SharedSelection = Arc<Mutex<Option<String>>>;

#[derive(Debug, Clone, Default)]
pub(super) struct CurrencyState {
    pub result: Option<CurrencyResult>,
    pub loading: bool,
    pub error: Option<String>,
    pub cache: HashMap<String, CachedRate>,
    pub pending_request: Option<CurrencyConversionRequest>,
    pub help: bool,
    pub multi_result: Option<MultiCurrencyResult>,
    pub multi_loading: bool,
    pub pending_multi_request: Option<MultiCurrencyRequest>,
}

impl CurrencyState {
    pub fn clear(&mut self) {
        self.result = None;
        self.loading = false;
        self.error = None;
        self.pending_request = None;
        self.help = false;
        self.multi_result = None;
        self.multi_loading = false;
        self.pending_multi_request = None;
    }

    pub fn clear_single(&mut self) {
        self.result = None;
        self.loading = false;
        self.error = None;
        self.pending_request = None;
    }

    pub fn clear_multi(&mut self) {
        self.multi_result = None;
        self.multi_loading = false;
        self.pending_multi_request = None;
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct ScriptFilterState {
    pub results: Option<ScriptFilterResult>,
    pub loading: bool,
    pub loading_name: Option<String>,
    pub generation: u64,
    pub action: Option<String>,
    pub secondary_action: Option<String>,
    pub help_message: Option<String>,
}

impl ScriptFilterState {
    pub fn clear(&mut self) {
        self.results = None;
        self.loading = false;
        self.loading_name = None;
        self.action = None;
        self.secondary_action = None;
        self.help_message = None;
    }
}

/// Clipboard contents used to render snippet previews.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClipboardSnapshot {
    pub fetching: bool,
    pub data: ClipboardData,
}

/// A snippet selection waiting for its placeholders to expand. The action is
/// captured when the user activates the snippet, so releasing Ctrl while the
/// expansion runs does not change what happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingSelection {
    pub action: String,
    pub preparing_paste: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TextSnippetState {
    pub items: Vec<TextSnippet>,
    pub filtered: Vec<usize>,
    pub active: bool,
    pub loading: bool,
    pub icon: Option<String>,
    pub action: Option<String>,
    pub secondary_action: Option<String>,
    pub generation: u64,
    pub file_cache: HashMap<String, Vec<TextSnippet>>,
    /// Config index of the source being shown.
    pub source_index: Option<usize>,
    /// Bumped whenever the source changes or snippets are closed, so late
    /// clipboard and expansion results can be recognised and dropped.
    pub session: u64,
    pub placeholders: bool,
    pub restore_clipboard: bool,
    pub allow_shell: bool,
    pub clipboard: ClipboardSnapshot,
    /// Clipboard manager setting for `{clipboard:N}`; kept across sessions.
    pub clipboard_history: crate::ClipboardHistory,
    /// Expanded subtitles, indexed like `items`.
    pub previews: Vec<Option<String>>,
    pub pending: Option<PendingSelection>,
    pub error: Option<String>,
}

impl TextSnippetState {
    pub fn clear(&mut self) {
        self.items.clear();
        self.filtered.clear();
        self.active = false;
        self.loading = false;
        self.icon = None;
        self.action = None;
        self.secondary_action = None;
        self.reset_session(None);
    }

    /// Start a new session for `source_index`, forgetting anything tied to
    /// the previous source.
    pub fn reset_session(&mut self, source_index: Option<usize>) {
        self.source_index = source_index;
        self.session = self.session.wrapping_add(1);
        // Drop late command results from the previous source too.
        self.generation = self.generation.wrapping_add(1);
        self.clipboard = ClipboardSnapshot::default();
        self.previews.clear();
        self.pending = None;
        self.error = None;
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct WebSearchState {
    pub active: Option<WebSearchActiveState>,
}

impl WebSearchState {
    pub fn clear(&mut self) {
        self.active = None;
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct FileBrowserState {
    pub entries: Vec<FileBrowserEntry>,
    pub all_entries: Vec<FileBrowserEntry>,
    pub active: bool,
    pub show_hidden: bool,
    /// Listing order, cycled with Ctrl+S; survives leaving the browser.
    pub sort: crate::FileBrowserSort,
    /// Flip the order of `sort` (from the `sort_reverse` setting).
    pub sort_reverse: bool,
    pub current_dir: String,
    pub error: Option<String>,
    pub cached_preview: Option<InspectedFile>,
    /// Entries index of the row under the pointer, for hover styling.
    pub hovered_row: Option<usize>,
    /// Display index of the row the left button went down on. A release on
    /// the same row activates it, like a button click.
    pub pressed_row: Option<usize>,
    /// A drag-and-drop session is in flight.
    pub dragging: bool,
}

impl FileBrowserState {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.all_entries.clear();
        self.active = false;
        self.current_dir.clear();
        self.error = None;
        self.cached_preview = None;
        self.hovered_row = None;
        self.pressed_row = None;
        self.dragging = false;
    }
}

#[derive(Debug, Clone)]
pub(super) struct ViewState {
    pub search_input_id: TextInputId,
    pub scrollable_id: ScrollableId,
    pub preview_scrollable_id: ScrollableId,
    pub items_container_id: ContainerId,
    pub generation: u64,
    pub current_modifiers: iced::keyboard::Modifiers,
    pub theme: ThemeColors,
    pub font_sizes: crate::ui::FontSizes,
    pub help_active: bool,
    pub show_preview: bool,
    pub show_launcher_preview: bool,
    pub preview_style: PreviewStyle,
    /// Live window size, updated on resize events, used to size image previews.
    pub window_size: iced::Size,
    pub default_script_shell: String,
}

impl ViewState {
    pub fn new(theme: ThemeColors, font_sizes: crate::ui::FontSizes) -> Self {
        Self {
            search_input_id: TextInputId::unique(),
            scrollable_id: ScrollableId::unique(),
            preview_scrollable_id: ScrollableId::unique(),
            items_container_id: ContainerId::unique(),
            generation: 0,
            current_modifiers: iced::keyboard::Modifiers::empty(),
            preview_style: PreviewStyle::resolve(&theme, &crate::PreviewConfig::default(), None),
            window_size: iced::Size::new(800.0, 600.0),
            theme,
            font_sizes,
            help_active: false,
            show_preview: true,
            show_launcher_preview: false,
            default_script_shell: "bash".to_string(),
        }
    }

    pub fn refresh_ids(&mut self) {
        self.scrollable_id = ScrollableId::unique();
        self.preview_scrollable_id = ScrollableId::unique();
        self.items_container_id = ContainerId::unique();
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct HistoryState {
    pub items: Vec<String>,
    pub index: Option<usize>,
    pub saved_query: String,
    pub search_in_progress: bool,
    pub max_items: u32,
}

#[derive(Default)]
pub(super) struct EmojiState {
    pub active: bool,
    pub data: Vec<EmojiEntry>,
    pub data_loading: bool,
    pub filtered: Vec<usize>,
    pub action: Option<String>,
    pub secondary_action: Option<String>,
    pub matcher: SkimMatcherV2,
}

impl EmojiState {
    pub fn clear(&mut self) {
        self.active = false;
        self.filtered.clear();
        self.action = None;
        self.secondary_action = None;
    }
}

#[derive(Debug, Clone)]
pub(super) enum FallbackAction {
    WebSearch {
        url_template: String,
    },
    ScriptFilter {
        #[allow(dead_code)]
        keyword: String,
        command: String,
        args: Vec<String>,
        action: Option<String>,
        #[allow(dead_code)]
        secondary_action: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedFallback {
    pub name: String,
    pub icon: Option<String>,
    pub action: FallbackAction,
}

/// A keyword row offered while the user is still typing a keyword, before the
/// space that arms the addon. Selecting it completes the input to `keyword `.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct KeywordSuggestion {
    pub keyword: String,
    pub subtitle: String,
    pub icon: Option<String>,
}

pub(super) struct LauncherApp {
    pub configs: Vec<crate::RaffiConfig>,
    pub filtered_configs: Vec<usize>,
    pub search_query: String,
    pub selected_index: usize,
    pub selected_item: SharedSelection,
    pub icon_map: HashMap<String, String>,
    pub no_icons: bool,
    pub launcher_icon_paths: Vec<Option<String>>,
    pub mru_map: HashMap<String, super::support::MruEntry>,
    pub addons: AddonsConfig,
    pub calculator_result: Option<CalculatorResult>,
    pub currency: CurrencyState,
    pub script_filter: ScriptFilterState,
    pub text_snippets: TextSnippetState,
    pub hyprland: Option<Result<super::hyprland::Hyprland, String>>,
    pub deferred_paste: Arc<Mutex<Option<super::hyprland::Hyprland>>>,
    pub web_search: WebSearchState,
    pub file_browser: FileBrowserState,
    /// Wayland drag-and-drop source worker, if the UI runs on Wayland.
    pub drag: Option<super::dnd::DragSource>,
    pub view: ViewState,
    pub history: HistoryState,
    pub emoji: EmojiState,
    pub fallbacks: Vec<ResolvedFallback>,
    pub keyword_suggestions: Vec<KeywordSuggestion>,
    pub help_entries: Vec<HelpEntry>,
    pub help_filtered: Vec<usize>,
}
