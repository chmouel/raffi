use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fuzzy_matcher::skim::SkimMatcherV2;
use iced::widget::Id;

use super::help::HelpEntry;
use super::preview::InspectedFile;
use super::theme::{PreviewStyle, ThemeColors};
pub(crate) use super::types::{
    CachedRate, CalculatorResult, ClipboardData, CurrencyConversion, CurrencyConversionRequest,
    CurrencyResult, DynamicPreview, EmojiEntry, FileBrowserEntry, Message, MultiCurrencyRequest,
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
    /// Config index of the script filter that produced the current results.
    pub config_index: Option<usize>,
    pub query: String,
    /// Bumped on every selection or result change so stale preview work is
    /// dropped.
    pub preview_generation: u64,
    /// Copy of `preview_generation` read by sleeping refresh timers, so a
    /// superseded timer ends early instead of holding its thread.
    pub preview_live_generation: Arc<AtomicU64>,
    /// Dynamic previews of the current results, by item index.
    pub previews: HashMap<usize, DynamicPreview>,
    /// Config indexes whose script did not answer `RAFFI_PREVIEW=1` with a
    /// preview object; kept for the whole session.
    pub preview_unsupported: HashSet<usize>,
    /// Config indexes whose script answered with a preview object at least once.
    pub preview_supported: HashSet<usize>,
    /// Config indexes with a first preview run in flight. Until it answers no
    /// other run starts, so a legacy script is run only once more.
    pub preview_probing: HashSet<usize>,
}

impl ScriptFilterState {
    pub fn clear(&mut self) {
        self.results = None;
        self.loading = false;
        self.loading_name = None;
        self.action = None;
        self.secondary_action = None;
        self.help_message = None;
        self.config_index = None;
        self.query.clear();
        self.previews.clear();
        self.bump_preview_generation();
    }

    pub fn bump_preview_generation(&mut self) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        self.preview_live_generation
            .store(self.preview_generation, Ordering::Relaxed);
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
    /// Snippet usage keyed by `snippet_mru_key`; kept across sessions.
    pub mru_map: HashMap<String, super::support::MruEntry>,
    /// Where `mru_map` is saved; `None` keeps usage in memory only.
    pub mru_path: Option<std::path::PathBuf>,
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

/// Clipboard history browser. Items come from CopyQ page by page; each scan
/// has its own generation so pages from an older scan are dropped.
#[derive(Debug, Clone, Default)]
pub(super) struct ClipboardState {
    pub active: bool,
    pub loading: bool,
    pub generation: u64,
    /// Bumped when the browser opens or closes, to drop late previews and
    /// action results.
    pub session: u64,
    pub query: String,
    /// List only image items.
    pub images_only: bool,
    /// Whether the images keyword opened the browser; Ctrl+I overrides
    /// `images_only` until the keyword changes.
    pub image_keyword: bool,
    /// Tab, total row count and newest item seen by the current scan.
    pub tab: String,
    pub total: usize,
    pub head: String,
    /// Rows scanned so far by the current scan.
    pub scanned: usize,
    /// Sorted by match tier then row, one entry per item id.
    pub items: Vec<super::clipboard::ClipItem>,
    /// The next page starts a new scan and replaces `items`.
    pub fresh: bool,
    pub restarts: u32,
    pub error: Option<String>,
    /// Item id to keep selected while results move around.
    pub follow: Option<String>,
    /// Recent previews by item id, oldest first.
    pub previews: Vec<(String, Result<super::clipboard::ClipPreview, String>)>,
    pub preview_inflight: Option<String>,
    /// An action is running; further activations are ignored.
    pub pending: bool,
}

impl ClipboardState {
    pub fn clear(&mut self) {
        let generation = self.generation.wrapping_add(1);
        let session = self.session.wrapping_add(1);
        *self = Self {
            generation,
            session,
            ..Self::default()
        };
    }

    /// Rows the browser adds to the list: one status row while nothing is
    /// listed.
    pub fn row_count(&self) -> usize {
        if !self.active {
            0
        } else {
            self.items.len().max(1)
        }
    }

    pub fn preview(&self, id: &str) -> Option<&Result<super::clipboard::ClipPreview, String>> {
        self.previews
            .iter()
            .find(|(cached, _)| cached == id)
            .map(|(_, preview)| preview)
    }

    /// Message shown in place of the list when it is empty.
    pub fn status(&self) -> String {
        if let Some(error) = &self.error {
            error.clone()
        } else if self.images_only {
            if self.loading {
                "Searching clipboard images...".into()
            } else if self.query.is_empty() {
                "No images in clipboard history".into()
            } else {
                "No clipboard images match".into()
            }
        } else if self.loading {
            if self.query.is_empty() {
                "Loading clipboard history...".into()
            } else {
                "Searching clipboard history...".into()
            }
        } else if self.query.is_empty() {
            "Clipboard history is empty".into()
        } else {
            "No clipboard entries match".into()
        }
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
    /// Files/directories filter, cycled with Ctrl+.; survives leaving the
    /// browser.
    pub show: crate::FileBrowserShow,
    /// Best match for the typed text among entries hidden by `show`, so Tab
    /// can still complete it when no visible row matches.
    pub hidden_match: Option<FileBrowserEntry>,
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
    /// Entry the `preview_command` should describe: the selection, while the
    /// preview pane is shown.
    pub preview_target: Option<String>,
    /// The debounce for `preview_target` is over.
    pub preview_due: bool,
    /// Resolved preview command; None when previews are off or python3
    /// is missing for the bundled script.
    pub preview_command: Option<super::file_preview::FilePreviewCommand>,
    /// A `preview_command` run is out; only one runs at a time.
    pub preview_inflight: bool,
    /// `preview_command` results, oldest first; kept for the session.
    pub script_previews: Vec<(
        super::file_preview::FilePreviewKey,
        super::file_preview::FilePreviewResult,
    )>,
}

impl FileBrowserState {
    pub fn clear(&mut self) {
        self.entries.clear();
        self.all_entries.clear();
        self.hidden_match = None;
        self.active = false;
        self.current_dir.clear();
        self.error = None;
        self.cached_preview = None;
        self.hovered_row = None;
        self.pressed_row = None;
        self.dragging = false;
        self.preview_target = None;
        self.preview_due = false;
    }

    /// Latest `preview_command` result for `path`.
    pub fn script_preview(&self, path: &str) -> Option<&super::file_preview::FilePreviewResult> {
        self.script_previews
            .iter()
            .rev()
            .find(|(key, _)| key.path == path)
            .map(|(_, result)| result)
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
    /// Text shown in the empty search box.
    pub placeholder: String,
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
            placeholder: super::help::DEFAULT_PLACEHOLDER.to_string(),
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

#[derive(Default)]
pub(super) struct DevtoolsState {
    pub active: bool,
    pub rows: Vec<super::devtools::DevRow>,
    pub loading: bool,
    /// Bumped by every `dev` query and when leaving the mode; a hash worker
    /// started for an older value skips its work or has its result dropped.
    pub hash_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Hash command replacing every hash tool in tests.
    #[cfg(test)]
    pub hash_command: Option<Vec<String>>,
}

impl DevtoolsState {
    pub fn clear(&mut self) {
        self.active = false;
        self.rows.clear();
        self.loading = false;
        self.hash_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn current_generation(&self) -> u64 {
        self.hash_generation
            .load(std::sync::atomic::Ordering::SeqCst)
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
    /// Run the query in the terminal `prefix` starts.
    Terminal {
        prefix: Vec<String>,
        hold: bool,
    },
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedFallback {
    pub name: String,
    pub icon: Option<String>,
    pub action: FallbackAction,
}

impl ResolvedFallback {
    /// The row title for `query`.
    pub fn label(&self, query: &str) -> String {
        match self.action {
            FallbackAction::Terminal { .. } => format!("Run '{query}' in {}", self.name),
            _ => format!("Search {} for '{query}'", self.name),
        }
    }
}

/// A keyword row offered while the user is still typing a keyword, before the
/// space that arms the addon. Selecting it completes the input to `keyword `.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct KeywordSuggestion {
    pub keyword: String,
    pub subtitle: String,
    pub icon: Option<String>,
}

/// Actions still running, keyed by provider and target. Query and mode
/// changes keep them so the same action cannot be submitted twice; only the
/// action's completion releases it.
#[derive(Debug, Default)]
pub(super) struct InFlightActions {
    keys: HashSet<(&'static str, String)>,
}

impl InFlightActions {
    /// Mark the action as running; `false` when it already is.
    pub fn begin(&mut self, provider: &'static str, target: &str) -> bool {
        self.keys.insert((provider, target.to_string()))
    }

    pub fn finish(&mut self, provider: &'static str, target: &str) {
        self.keys.remove(&(provider, target.to_string()));
    }

    pub fn is_pending(&self, provider: &'static str, target: &str) -> bool {
        self.keys.contains(&(provider, target.to_string()))
    }
}

pub(super) struct LauncherApp {
    pub configs: Vec<crate::RaffiConfig>,
    pub filtered_configs: Vec<usize>,
    pub search_query: String,
    pub select_1_query: Option<String>,
    pub select_1_ready: bool,
    pub selected_index: usize,
    pub selected_item: SharedSelection,
    pub icon_map: HashMap<String, String>,
    /// Desktop entry icons keyed by executable name.
    pub desktop_icon_map: HashMap<String, String>,
    pub no_icons: bool,
    pub launcher_icon_paths: Vec<Option<String>>,
    pub mru_map: HashMap<String, super::support::MruEntry>,
    pub sort_mode: crate::SortMode,
    pub addons: AddonsConfig,
    pub query_class: super::app::QueryClass,
    pub inline_result: Option<super::types::InlineResult>,
    pub currency: CurrencyState,
    pub script_filter: ScriptFilterState,
    pub text_snippets: TextSnippetState,
    pub hyprland: Option<Result<super::hyprland::Hyprland, String>>,
    pub deferred_action: Arc<Mutex<Option<super::actions::DeferredAction>>>,
    pub in_flight: InFlightActions,
    /// Bumped whenever the query or mode changes, so an inline copy that
    /// completes afterwards is recognised as stale.
    pub inline_generation: u64,
    /// Shown below the inline rows when copying one of them failed.
    pub inline_copy_error: Option<String>,
    /// Clipboard command replacing the real copier in tests.
    #[cfg(test)]
    pub copier: Option<Vec<String>>,
    /// Reports whether a program is installed; replaced in tests.
    pub program_exists: fn(&str) -> bool,
    /// Detects the compositor Raffi runs in; replaced in tests.
    pub compositor: fn() -> Option<super::windows::Compositor>,
    /// Finds the running terminal to open a tab in; replaced in tests.
    pub terminal_tab: fn(&[String], Option<&str>) -> Option<crate::terminal::TabTarget>,
    pub web_search: WebSearchState,
    pub file_browser: FileBrowserState,
    /// Wayland drag-and-drop source worker, if the UI runs on Wayland.
    pub drag: Option<super::dnd::DragSource>,
    pub view: ViewState,
    pub history: HistoryState,
    pub emoji: EmojiState,
    pub command_list: super::command_list::CommandListState,
    pub vault: super::vault::VaultState,
    pub slack: super::slack_mode::SlackState,
    pub devtools: DevtoolsState,
    pub clipboard: ClipboardState,
    pub fallbacks: Vec<ResolvedFallback>,
    /// Addon rows mixed into the standard results (`general.main_view`).
    pub main_view: super::main_view::MainViewState,
    pub keyword_suggestions: Vec<KeywordSuggestion>,
    pub help_entries: Vec<HelpEntry>,
    pub help_filtered: Vec<usize>,
    pub help_saved_query: Option<String>,
}
