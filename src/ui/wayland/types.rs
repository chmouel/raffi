use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::TextSnippet;

/// Calculator result for math expression evaluation.
#[derive(Debug, Clone)]
pub(crate) struct CalculatorResult {
    pub expression: String,
    pub result: f64,
}

/// Parser that owns the inline result area for a standard-mode query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlineProvider {
    Calculator,
    Currency,
    Units,
    Color,
    Time,
    Uuid,
    Password,
}

/// One row shown above the launchers; Enter copies `copy_value`, Ctrl+Enter
/// `alt_copy_value` when set.
#[derive(Debug, Clone)]
pub(crate) struct InlineRow {
    pub title: String,
    pub subtitle: Option<String>,
    pub copy_value: String,
    pub alt_copy_value: Option<String>,
    pub swatch: Option<iced::Color>,
    pub preview: Option<super::preview::PreviewData>,
}

#[derive(Debug, Clone)]
pub(crate) struct InlineResult {
    pub provider: InlineProvider,
    pub rows: Vec<InlineRow>,
}

/// Currency conversion request parsed from user input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CurrencyConversionRequest {
    pub amount: f64,
    pub from_currency: String,
    pub to_currency: String,
}

/// Result of a currency conversion.
#[derive(Debug, Clone)]
pub(crate) struct CurrencyResult {
    pub request: CurrencyConversionRequest,
    pub converted_amount: f64,
    pub rate: f64,
}

/// Multi-currency conversion request (simple syntax without to/in).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MultiCurrencyRequest {
    pub amount: f64,
    pub from_currency: String,
    pub to_currencies: Vec<String>,
}

/// Result of multi-currency conversion.
#[derive(Debug, Clone)]
pub(crate) struct MultiCurrencyResult {
    pub amount: f64,
    pub from_currency: String,
    pub conversions: Vec<CurrencyConversion>,
}

/// Single currency conversion within a multi-currency result.
#[derive(Debug, Clone)]
pub(crate) struct CurrencyConversion {
    pub to_currency: String,
    pub converted_amount: f64,
    pub rate: f64,
}

/// Alfred Script Filter JSON icon.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ScriptFilterIcon {
    pub path: Option<String>,
}

/// Alfred Script Filter JSON text object (`copy` and `largetype`).
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub(crate) struct ScriptFilterItemText {
    pub copy: Option<String>,
    pub largetype: Option<String>,
}

/// Alfred Script Filter JSON item.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ScriptFilterItem {
    pub title: String,
    pub subtitle: Option<String>,
    pub arg: Option<String>,
    pub icon: Option<ScriptFilterIcon>,
    /// Optional explicit preview text or local file/image path for the right-hand preview pane.
    #[serde(default)]
    pub preview: Option<String>,
    /// Optional Alfred QuickLook URL or local file path.
    #[serde(default)]
    pub quicklookurl: Option<String>,
    /// Optional Alfred `text` object (`copy` and `largetype`).
    #[serde(default)]
    pub text: Option<ScriptFilterItemText>,
}

/// Container for script filter results with default icon.
#[derive(Debug, Clone)]
pub(crate) struct ScriptFilterResult {
    pub items: Vec<ScriptFilterItem>,
    pub default_icon: Option<String>,
}

/// A file or directory entry for the file browser addon.
#[derive(Debug, Clone)]
pub(crate) struct FileBrowserEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
}

/// Active web search state when a web search keyword is matched.
#[derive(Debug, Clone)]
pub(crate) struct WebSearchActiveState {
    pub name: String,
    pub query: String,
    pub url_template: String,
    pub icon: Option<String>,
}

/// Cached exchange rate with timestamp for TTL.
#[derive(Debug, Clone)]
pub(crate) struct CachedRate {
    pub rate: f64,
    pub timestamp: Instant,
}

impl CachedRate {
    const TTL: Duration = Duration::from_secs(3600);

    pub(crate) fn new(rate: f64) -> Self {
        Self {
            rate,
            timestamp: Instant::now(),
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.timestamp.elapsed() < Self::TTL
    }
}

/// An emoji or nerd-font icon entry loaded at runtime from CSV data files.
#[derive(Debug, Clone)]
pub(crate) struct EmojiEntry {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    SearchChanged(String),
    MoveUp,
    MoveDown,
    Submit,
    /// Enter held down; never confirms an armed command list action.
    SubmitRepeat,
    Cancel,
    ItemClicked(usize),
    InlineSelected(usize),
    /// A checked copy of an inline or currency row started at the given
    /// `inline_generation` finished.
    InlineCopied(u64, std::result::Result<(), String>),
    CurrencyConversionResult(
        CurrencyConversionRequest,
        std::result::Result<CurrencyResult, String>,
    ),
    CurrencyResultCopied,
    MultiCurrencyConversionResult(
        MultiCurrencyRequest,
        std::result::Result<MultiCurrencyResult, String>,
    ),
    MultiCurrencyResultCopied(usize),
    ScriptFilterResult(u64, std::result::Result<ScriptFilterResult, String>),
    ScriptFilterItemSelected(usize),
    TextSnippetCommandResult(u64, std::result::Result<Vec<TextSnippet>, String>),
    TextSnippetSelected(usize),
    TextSnippetClipboard(u64, ClipboardData),
    TextSnippetExpanded(
        u64,
        std::result::Result<super::placeholders::Expanded, String>,
    ),
    HyprPastePrepared(u64, std::result::Result<super::hyprland::Hyprland, String>),
    WebSearchSelected,
    FileBrowserItemSelected(usize),
    /// The user pressed the left button over a file browser row; carries the
    /// display index and the entry path so a drag can be armed.
    FileBrowserRowPressed {
        index: usize,
        path: String,
    },
    /// The user released the left button over a file browser row.
    FileBrowserRowReleased(usize),
    /// The pointer entered the file browser row at this entries index.
    FileBrowserHover(usize),
    /// The pointer left the file browser row at this entries index.
    FileBrowserUnhover(usize),
    /// A left button or touch release anywhere; used to reset row press
    /// tracking for mouse and touch.
    MouseReleased,
    /// Raw Wayland handles of the main window, gathered once at startup.
    WaylandHandles(Option<super::dnd::RawWaylandHandles>),
    /// Lifecycle of a drag-and-drop session started from the file browser.
    DragEvent(super::dnd::DragEvent),
    TabComplete,
    FileBrowserToggleHidden,
    /// Open the directory being browsed in a file manager.
    FileBrowserOpenFileManager,
    /// Switch the file browser to the next sort mode.
    FileBrowserCycleSort,
    ModifiersChanged(iced::keyboard::Modifiers),
    HistoryPrevious,
    HistoryNext,
    ToggleHelp,
    TogglePreview,
    PreviewScrollDown,
    PreviewScrollUp,
    WindowResized(iced::Size),
    HelpEntrySelected(usize),
    EmojiSelected(usize),
    EmojiDataLoaded(Vec<EmojiEntry>),
    CommandListSelected(usize),
    /// Items loaded for the command list with this generation.
    CommandListLoaded(
        u64,
        std::result::Result<Vec<super::command_list::CommandItem>, String>,
    ),
    DevtoolsSelected(usize),
    /// Hash rows computed for the `dev` query with this generation.
    DevtoolsHashes(u64, Vec<super::devtools::DevRow>),
    FallbackSelected(usize),
    KeywordSuggestionSelected(usize),
    /// A page of clipboard history for the scan with this generation.
    ClipboardPage(u64, std::result::Result<super::clipboard::Page, String>),
    /// Preview of the item with this id, for the given browser session.
    ClipboardPreview(
        u64,
        String,
        std::result::Result<super::clipboard::ClipPreview, String>,
    ),
    ClipboardSelected(usize),
    /// A copy finished; carries the paste target when pasting.
    ClipboardActionDone(
        u64,
        std::result::Result<Option<super::hyprland::Hyprland>, String>,
    ),
    ClipboardRefresh,
    ClipboardToggleImages,
}

/// Clipboard text fetched for snippet previews. `None` or a missing history
/// index means it has not been fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClipboardData {
    pub current: Option<std::result::Result<String, String>>,
    pub history: std::collections::BTreeMap<usize, std::result::Result<String, String>>,
}

impl ClipboardData {
    pub fn merge(&mut self, other: ClipboardData) {
        if other.current.is_some() {
            self.current = other.current;
        }
        self.history.extend(other.history);
    }
}
