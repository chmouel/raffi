use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::TextSnippet;

/// Calculator result for math expression evaluation.
#[derive(Debug, Clone)]
pub(crate) struct CalculatorResult {
    pub expression: String,
    pub result: f64,
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

/// Alfred Script Filter JSON item.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ScriptFilterItem {
    pub title: String,
    pub subtitle: Option<String>,
    pub arg: Option<String>,
    pub icon: Option<ScriptFilterIcon>,
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
    Cancel,
    ItemClicked(usize),
    CalculatorSelected,
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
    WebSearchSelected,
    FileBrowserItemSelected(usize),
    FileBrowserTabComplete,
    FileBrowserToggleHidden,
    ModifiersChanged(iced::keyboard::Modifiers),
    HistoryPrevious,
    HistoryNext,
    ToggleHints,
    EmojiSelected(usize),
    EmojiDataLoaded(Vec<EmojiEntry>),
}
