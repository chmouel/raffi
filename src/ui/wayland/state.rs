use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fuzzy_matcher::skim::SkimMatcherV2;
use iced::widget::Id;

use super::theme::ThemeColors;
pub(crate) use super::types::{
    CachedRate, CalculatorResult, CurrencyConversion, CurrencyConversionRequest, CurrencyResult,
    EmojiEntry, FileBrowserEntry, Message, MultiCurrencyRequest, MultiCurrencyResult,
    ScriptFilterResult, WebSearchActiveState,
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
    pub current_dir: String,
    pub error: Option<String>,
}

impl FileBrowserState {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.all_entries.clear();
        self.active = false;
        self.current_dir.clear();
        self.error = None;
    }
}

#[derive(Debug, Clone)]
pub(super) struct ViewState {
    pub search_input_id: TextInputId,
    pub scrollable_id: ScrollableId,
    pub items_container_id: ContainerId,
    pub generation: u64,
    pub current_modifiers: iced::keyboard::Modifiers,
    pub theme: ThemeColors,
    pub font_sizes: crate::ui::FontSizes,
    pub show_hints: bool,
}

impl ViewState {
    pub fn new(theme: ThemeColors, font_sizes: crate::ui::FontSizes) -> Self {
        Self {
            search_input_id: TextInputId::unique(),
            scrollable_id: ScrollableId::unique(),
            items_container_id: ContainerId::unique(),
            generation: 0,
            current_modifiers: iced::keyboard::Modifiers::empty(),
            theme,
            font_sizes,
            show_hints: false,
        }
    }

    pub fn refresh_ids(&mut self) {
        self.scrollable_id = ScrollableId::unique();
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

pub(super) struct LauncherApp {
    pub configs: Vec<crate::RaffiConfig>,
    pub filtered_configs: Vec<usize>,
    pub search_query: String,
    pub selected_index: usize,
    pub selected_item: SharedSelection,
    pub icon_map: HashMap<String, String>,
    pub mru_map: HashMap<String, super::support::MruEntry>,
    pub addons: AddonsConfig,
    pub calculator_result: Option<CalculatorResult>,
    pub currency: CurrencyState,
    pub script_filter: ScriptFilterState,
    pub text_snippets: TextSnippetState,
    pub web_search: WebSearchState,
    pub file_browser: FileBrowserState,
    pub view: ViewState,
    pub history: HistoryState,
    pub emoji: EmojiState,
}
