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

/// Image in a Script Filter preview: a path string, `{"path": ...}`, or
/// `{"base64": ...}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawPreviewImage {
    Path(String),
    Source {
        path: Option<String>,
        base64: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RawPreviewSection {
    Text {
        heading: Option<String>,
        content: String,
    },
    Image {
        path: Option<String>,
        base64: Option<String>,
    },
    File {
        path: String,
    },
}

/// Rich Script Filter preview, as written by a script in an item's `preview`
/// object or in a `RAFFI_PREVIEW=1` response.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(crate) struct RawPreviewSpec {
    title: Option<String>,
    subtitle: Option<String>,
    badge: Option<String>,
    icon: Option<String>,
    text: Option<String>,
    image: Option<RawPreviewImage>,
    sections: Option<Vec<RawPreviewSection>>,
    metadata: Vec<(String, String)>,
    refresh: Option<f64>,
    dynamic: Option<bool>,
}

/// Why a `RAFFI_PREVIEW=1` run gave no preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScriptFilterPreviewError {
    /// The script failed, timed out or could not be started.
    Run(String),
    /// The script answered with a preview object that cannot be read.
    Invalid(String),
}

impl ScriptFilterPreviewError {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Run(message) | Self::Invalid(message) => message,
        }
    }
}

/// Shortest allowed auto-refresh interval for dynamic previews.
pub(crate) const MIN_PREVIEW_REFRESH: Duration = Duration::from_millis(500);
/// Longest auto-refresh interval, in seconds; larger values are clamped.
const MAX_PREVIEW_REFRESH_SECS: f64 = 3600.0;

/// One body block of a prepared Script Filter preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreparedPreviewBody {
    Text {
        heading: Option<String>,
        content: String,
    },
    Image(String),
    ImageData(iced::widget::image::Handle),
    File(String),
}

/// Script Filter preview with images decoded once, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PreparedPreview {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub badge: Option<String>,
    pub icon: Option<String>,
    pub bodies: Vec<PreparedPreviewBody>,
    /// Whether `bodies` replaces the default preview body, even when empty
    /// (an explicit `"sections": []`).
    pub replaces_body: bool,
    pub metadata: Vec<(String, String)>,
    pub refresh: Option<Duration>,
    /// `false` when the item opts out of the dynamic preview call.
    pub dynamic: bool,
}

impl PreparedPreview {
    /// Path of the first `file` section, which is the only one rendered.
    pub fn file_path(&self) -> Option<&str> {
        self.bodies.iter().find_map(|body| match body {
            PreparedPreviewBody::File(path) => Some(path.as_str()),
            _ => None,
        })
    }
}

fn prepare_image(path: Option<String>, data: Option<String>) -> Option<PreparedPreviewBody> {
    use base64::Engine;
    if let Some(data) = data {
        // Accept data URLs as well as bare base64.
        let encoded = data
            .split_once(";base64,")
            .map_or(data.as_str(), |(_, rest)| rest);
        let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
        return Some(
            match base64::engine::general_purpose::STANDARD.decode(cleaned) {
                Ok(bytes) => {
                    PreparedPreviewBody::ImageData(iced::widget::image::Handle::from_bytes(bytes))
                }
                Err(error) => PreparedPreviewBody::Text {
                    heading: Some("Image".to_string()),
                    content: format!("Invalid base64 image: {error}"),
                },
            },
        );
    }
    path.map(PreparedPreviewBody::Image)
}

impl From<RawPreviewSpec> for PreparedPreview {
    fn from(raw: RawPreviewSpec) -> Self {
        let replaces_body = raw.sections.is_some() || raw.text.is_some() || raw.image.is_some();
        let bodies = if let Some(sections) = raw.sections {
            sections
                .into_iter()
                .filter_map(|section| match section {
                    RawPreviewSection::Text { heading, content } => {
                        Some(PreparedPreviewBody::Text { heading, content })
                    }
                    RawPreviewSection::Image { path, base64 } => prepare_image(path, base64),
                    RawPreviewSection::File { path } => Some(PreparedPreviewBody::File(path)),
                })
                .collect()
        } else {
            let image = raw.image.and_then(|image| match image {
                RawPreviewImage::Path(path) => prepare_image(Some(path), None),
                RawPreviewImage::Source { path, base64 } => prepare_image(path, base64),
            });
            let text = raw.text.map(|content| PreparedPreviewBody::Text {
                heading: None,
                content,
            });
            image.into_iter().chain(text).collect()
        };
        Self {
            title: raw.title,
            subtitle: raw.subtitle,
            badge: raw.badge,
            icon: raw.icon,
            bodies,
            replaces_body,
            metadata: raw.metadata,
            refresh: raw
                .refresh
                .filter(|secs| secs.is_finite() && *secs > 0.0)
                .map(|secs| {
                    Duration::from_secs_f64(secs.min(MAX_PREVIEW_REFRESH_SECS))
                        .max(MIN_PREVIEW_REFRESH)
                }),
            dynamic: raw.dynamic.unwrap_or(true),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RawItemPreview {
    Text(String),
    Rich(Box<RawPreviewSpec>),
}

/// Static per-item preview: plain text/path string or a rich object.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "RawItemPreview")]
pub(crate) enum ScriptFilterPreview {
    Text(String),
    Rich(Box<PreparedPreview>),
}

impl From<RawItemPreview> for ScriptFilterPreview {
    fn from(raw: RawItemPreview) -> Self {
        match raw {
            RawItemPreview::Text(text) => Self::Text(text),
            RawItemPreview::Rich(spec) => Self::Rich(Box::new((*spec).into())),
        }
    }
}

impl ScriptFilterPreview {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Rich(_) => None,
        }
    }

    pub fn as_rich(&self) -> Option<&PreparedPreview> {
        match self {
            Self::Text(_) => None,
            Self::Rich(preview) => Some(preview),
        }
    }
}

/// Alfred Script Filter JSON item.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ScriptFilterItem {
    pub title: String,
    #[serde(default = "crate::default_true")]
    pub valid: bool,
    pub subtitle: Option<String>,
    pub arg: Option<String>,
    pub icon: Option<ScriptFilterIcon>,
    /// Tree-view indentation level: each level shifts the row further right
    /// (default 0). Raffi extension, ignored by Alfred.
    #[serde(default)]
    pub indent: u32,
    /// Optional preview for the right-hand pane: text or a local file/image
    /// path, or a rich preview object.
    #[serde(default)]
    pub preview: Option<ScriptFilterPreview>,
    /// Alfred `variables`, passed as environment to the preview call and to
    /// shell actions.
    #[serde(default)]
    pub variables: std::collections::HashMap<String, String>,
    /// Optional Alfred QuickLook URL or local file path.
    #[serde(default)]
    pub quicklookurl: Option<String>,
    /// Optional Alfred `text` object (`copy` and `largetype`).
    #[serde(default)]
    pub text: Option<ScriptFilterItemText>,
}

/// Latest dynamic preview state of one Script Filter result item.
#[derive(Debug, Clone, Default)]
pub(crate) struct DynamicPreview {
    pub preview: Option<PreparedPreview>,
    pub error: Option<String>,
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
    /// Time to run the dynamic preview of this result item for this
    /// preview generation (after the debounce or refresh delay).
    ScriptFilterPreviewDue(u64, usize),
    /// Dynamic preview output for this generation, script filter config index
    /// and result item; `Ok(None)` when the script does not answer with a
    /// preview object.
    ScriptFilterPreview(
        u64,
        usize,
        usize,
        std::result::Result<Option<PreparedPreview>, ScriptFilterPreviewError>,
    ),
    /// The debounce for previewing this file browser path is over.
    FileBrowserPreviewDue(String),
    /// Output of the file browser `preview_command` for this path.
    FileBrowserPreview(
        super::file_preview::FilePreviewKey,
        super::file_preview::FilePreviewResult,
    ),
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
    /// A left button or touch release anywhere; used to reset row press
    /// tracking for mouse and touch.
    MouseReleased,
    /// Raw Wayland handles of the main window, gathered once at startup.
    WaylandHandles(Option<super::dnd::RawWaylandHandles>),
    /// Lifecycle of a drag-and-drop session started from the file browser.
    DragEvent(super::dnd::DragEvent),
    TabComplete,
    /// Ctrl+Tab (backwards with Shift): switch to the next view of what is
    /// showing, such as a main view source or a clipboard filter.
    CycleView(bool),
    FileBrowserToggleHidden,
    FileBrowserCopyPath,
    FileBrowserCopyImage,
    FileBrowserCopyCompleted(String, std::result::Result<(), String>),
    /// Open the selected entry with xdg-open, directories included.
    FileBrowserOpenSelected,
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
    /// An action run with Raffi kept open finished for this target, in the
    /// list session it started from.
    CommandListActionDone(
        super::command_list::CommandListAddon,
        u64,
        String,
        std::result::Result<(), String>,
    ),
    /// Site icons were downloaded for the browser tab list of this session;
    /// `true` when any arrived.
    FaviconsFetched(u64, bool),
    /// The bookmarks read for the command list load of this generation;
    /// None when reading stopped unexpectedly.
    BookmarksLoaded(u64, Option<std::sync::Arc<super::bookmarks::Library>>),
    /// Site icons by host for the bookmarks list of this session; `true`
    /// when missing icons should be downloaded next.
    BookmarkIconsLoaded(
        u64,
        std::collections::HashMap<String, std::path::PathBuf>,
        bool,
    ),
    /// The browser history views discovered and the first one loaded for
    /// the command list load of this generation.
    BrowserHistoryInitialized(
        u64,
        Option<std::sync::Arc<super::browser_history::BrowserHistoryInit>>,
    ),
    /// One browser view read for the command list load of this generation.
    BrowserHistoryGroupLoaded(
        u64,
        String,
        Option<std::sync::Arc<super::browser_history::Library>>,
    ),
    /// Site icons by host for one browser view of this session; `true` when
    /// missing icons should be downloaded next.
    BrowserHistoryIconsLoaded(
        u64,
        String,
        std::collections::HashMap<String, std::path::PathBuf>,
        bool,
    ),
    /// The browser history search of this session and generation is due.
    BrowserHistorySearchDue(u64, u64),
    /// The preview of the page at the URL for the browser tab list of this
    /// session; None when the row was left before it was looked up.
    TabPreviewLoaded(u64, String, Option<Vec<super::preview::PreviewBody>>),
    /// The Slack search for these terms in this list session is due.
    SlackSearchDue(u64, String),
    /// The preview of the Slack row with this identity in this list
    /// session, for this preview request; None when the row was left before
    /// it loaded.
    SlackPreviewLoaded(u64, String, u64, Option<super::slack::Preview>),
    /// The picture batch with this request in this list session is due.
    SlackImagesDue(u64, u64),
    /// The pictures of a batch: list session, account epoch, request, the
    /// keys asked for, and what was downloaded.
    SlackImagesLoaded(u64, u64, u64, Vec<String>, super::slack::ImagesOutcome),
    /// The item list was scrolled to this relative offset.
    ListScrolled(f32),
    /// A message or thread in the Slack preview pane was clicked to open in
    /// the desktop app or browser.
    SlackPreviewOpen {
        desktop_url: Option<String>,
        browser_url: Option<String>,
    },
    /// Rows of the main view command list source at this index, from its
    /// load of this generation.
    MainViewLoaded(
        usize,
        u64,
        std::result::Result<Vec<super::command_list::CommandItem>, String>,
    ),
    /// Debounced Slack message search for a main-view source.
    MainViewSlackSearchDue(usize, u64, String),
    /// Snippets of the main view snippet source at this index.
    MainViewSnippetsLoaded(usize, std::result::Result<Vec<TextSnippet>, String>),
    /// A main view script filter run of this source and generation; None
    /// when a later query superseded it before it started.
    MainViewScriptFilter(
        usize,
        u64,
        Option<std::result::Result<ScriptFilterResult, String>>,
    ),
    DevtoolsSelected(usize),
    /// Hash rows computed for the `dev` query with this generation.
    DevtoolsHashes(u64, Vec<super::devtools::DevRow>),
    FallbackSelected(usize),
    KeywordSuggestionSelected(usize),
    /// Left or Right (backwards) over the keyword chips row.
    ChipMove(bool),
    ChipClicked(usize),
    /// A click on the "‹" or "›" edge of the chips row (backwards).
    ChipScroll(bool),
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
    /// The startup probe of the cache service answered, or not.
    CacheServerStatus(Option<super::cache::ServerInfo>),
    /// F5, or Ctrl+Shift+F5 to refresh every source: refresh through the
    /// cache service.
    ServerRefresh(bool),
    /// The cache service finished the refresh with this token.
    ServerRefreshDone(u64, std::result::Result<(), super::cache::CallError>),
    /// Browser history read again after a server refresh: list session, group key.
    ServerHistoryReloaded(
        u64,
        String,
        Option<std::sync::Arc<super::browser_history::Library>>,
    ),
    /// Ctrl+D: delete the clipboard entry, or run the row's close action.
    DeleteSelected,
    /// The compositor reported windows or workspaces changing while the
    /// window switcher is open.
    WindowsChanged,
    /// The open window of the launcher at this config index, looked up
    /// before starting it: focus it, or launch when there is none.
    LauncherRaiseChecked(usize, Option<super::actions::DeferredAction>),
    ClipboardDeleteDone(u64, std::result::Result<String, String>),
    /// Text typed in the masked password prompt, never shown by `Debug`.
    PromptChanged(crate::secret::Secret),
    /// A password manager operation started with this generation finished.
    VaultDone(u64, super::vault::Report),
    /// The Bitwarden notes search started with this generation found these
    /// rows.
    VaultSearchDone(u64, std::collections::HashSet<String>),
    /// Run this action on the selected password manager entry.
    VaultShortcut(super::vault::EntryAction),
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
