use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, move_cursor_to_end, snap_to};
use iced::widget::{
    button, container, image, rich_text, scrollable, span, svg, text, text_input, Column, Id, Row,
};
use iced::window;
use iced::{Element, Length, Task};
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

type ContainerId = Id;
type ScrollableId = Id;
type TextInputId = Id;

use super::UI;
use crate::{
    execute_web_search_url, read_icon_map, AddonsConfig, RaffiConfig, ThemeColorsConfig, ThemeMode,
};

// --- Theme Colors ---
#[derive(Debug, Clone, Copy)]
struct ThemeColors {
    bg_base: iced::Color,
    bg_input: iced::Color,
    accent: iced::Color,
    accent_hover: iced::Color,
    text_main: iced::Color,
    text_muted: iced::Color,
    selection_bg: iced::Color,
    border: iced::Color,
}

impl ThemeColors {
    fn dark() -> Self {
        Self {
            bg_base: iced::Color {
                r: 0.15,
                g: 0.16,
                b: 0.21,
                a: 0.95,
            },
            bg_input: iced::Color {
                r: 0.26,
                g: 0.27,
                b: 0.35,
                a: 1.0,
            },
            accent: iced::Color {
                r: 0.74,
                g: 0.57,
                b: 0.97,
                a: 1.0,
            },
            accent_hover: iced::Color {
                r: 0.54,
                g: 0.91,
                b: 0.99,
                a: 1.0,
            },
            text_main: iced::Color::WHITE,
            text_muted: iced::Color {
                r: 0.38,
                g: 0.44,
                b: 0.64,
                a: 1.0,
            },
            selection_bg: iced::Color {
                r: 0.27,
                g: 0.29,
                b: 0.36,
                a: 0.8,
            },
            border: iced::Color {
                r: 0.38,
                g: 0.44,
                b: 0.64,
                a: 0.5,
            },
        }
    }

    fn light() -> Self {
        // Rose Pine Dawn palette
        Self {
            bg_base: iced::Color::from_rgb(
                0xfa as f32 / 255.0,
                0xf4 as f32 / 255.0,
                0xed as f32 / 255.0,
            ),
            bg_input: iced::Color::from_rgb(
                0xff as f32 / 255.0,
                0xfa as f32 / 255.0,
                0xf3 as f32 / 255.0,
            ),
            accent: iced::Color::from_rgb(
                0x90 as f32 / 255.0,
                0x7a as f32 / 255.0,
                0xa9 as f32 / 255.0,
            ),
            accent_hover: iced::Color::from_rgb(
                0x56 as f32 / 255.0,
                0x94 as f32 / 255.0,
                0x9f as f32 / 255.0,
            ),
            text_main: iced::Color::from_rgb(
                0x57 as f32 / 255.0,
                0x52 as f32 / 255.0,
                0x79 as f32 / 255.0,
            ),
            text_muted: iced::Color::from_rgb(
                0x98 as f32 / 255.0,
                0x93 as f32 / 255.0,
                0xa5 as f32 / 255.0,
            ),
            selection_bg: iced::Color::from_rgb(
                0xdf as f32 / 255.0,
                0xda as f32 / 255.0,
                0xd9 as f32 / 255.0,
            ),
            border: iced::Color::from_rgb(
                0x79 as f32 / 255.0,
                0x75 as f32 / 255.0,
                0x93 as f32 / 255.0,
            ),
        }
    }

    fn from_mode(mode: &ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
        }
    }

    fn from_mode_with_overrides(mode: &ThemeMode, overrides: Option<&ThemeColorsConfig>) -> Self {
        let mut colors = Self::from_mode(mode);
        if let Some(ov) = overrides {
            if let Some(c) = ov.bg_base.as_deref().and_then(parse_hex_color) {
                colors.bg_base = c;
            }
            if let Some(c) = ov.bg_input.as_deref().and_then(parse_hex_color) {
                colors.bg_input = c;
            }
            if let Some(c) = ov.accent.as_deref().and_then(parse_hex_color) {
                colors.accent = c;
            }
            if let Some(c) = ov.accent_hover.as_deref().and_then(parse_hex_color) {
                colors.accent_hover = c;
            }
            if let Some(c) = ov.text_main.as_deref().and_then(parse_hex_color) {
                colors.text_main = c;
            }
            if let Some(c) = ov.text_muted.as_deref().and_then(parse_hex_color) {
                colors.text_muted = c;
            }
            if let Some(c) = ov.selection_bg.as_deref().and_then(parse_hex_color) {
                colors.selection_bg = c;
            }
            if let Some(c) = ov.border.as_deref().and_then(parse_hex_color) {
                colors.border = c;
            }
        }
        colors
    }
}

/// Parse a hex colour string into an iced Color.
/// Supports `#RGB`, `#RRGGBB`, and `#RRGGBBAA` formats.
/// Returns `None` for invalid input.
fn parse_hex_color(hex: &str) -> Option<iced::Color> {
    let hex = hex.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some(iced::Color::from_rgba8(r * 17, g * 17, b * 17, 1.0))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(iced::Color::from_rgba8(r, g, b, 1.0))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(iced::Color::from_rgba8(r, g, b, a as f32 / 255.0))
        }
        _ => None,
    }
}

/// Wayland UI implementation using iced
pub struct WaylandUI;

impl UI for WaylandUI {
    fn show(
        &self,
        configs: &[RaffiConfig],
        addons: &AddonsConfig,
        no_icons: bool,
        initial_query: Option<&str>,
        theme: &ThemeMode,
        theme_colors: Option<&ThemeColorsConfig>,
        max_history: u32,
    ) -> Result<String> {
        run_wayland_ui(
            configs,
            addons,
            no_icons,
            initial_query,
            theme,
            theme_colors,
            max_history,
        )
    }
}

/// Shared state for capturing the selected item
type SharedSelection = Arc<Mutex<Option<String>>>;

/// Calculator result for math expression evaluation
#[derive(Debug, Clone)]
struct CalculatorResult {
    expression: String,
    result: f64,
}

/// Currency conversion request parsed from user input
#[derive(Debug, Clone, PartialEq)]
struct CurrencyConversionRequest {
    amount: f64,
    from_currency: String,
    to_currency: String,
}

/// Result of a currency conversion
#[derive(Debug, Clone)]
struct CurrencyResult {
    request: CurrencyConversionRequest,
    converted_amount: f64,
    rate: f64,
}

/// Multi-currency conversion request (simple syntax without to/in)
#[derive(Debug, Clone, PartialEq)]
struct MultiCurrencyRequest {
    amount: f64,
    from_currency: String,
    to_currencies: Vec<String>,
}

/// Result of multi-currency conversion
#[derive(Debug, Clone)]
struct MultiCurrencyResult {
    amount: f64,
    from_currency: String,
    conversions: Vec<CurrencyConversion>,
}

/// Single currency conversion within a multi-currency result
#[derive(Debug, Clone)]
struct CurrencyConversion {
    to_currency: String,
    converted_amount: f64,
    rate: f64,
}

/// Alfred Script Filter JSON icon
#[derive(Debug, Clone, Deserialize)]
struct ScriptFilterIcon {
    path: Option<String>,
}

/// Alfred Script Filter JSON item
#[derive(Debug, Clone, Deserialize)]
struct ScriptFilterItem {
    title: String,
    subtitle: Option<String>,
    arg: Option<String>,
    icon: Option<ScriptFilterIcon>,
}

/// Alfred Script Filter JSON response
#[derive(Debug, Clone, Deserialize)]
struct ScriptFilterResponse {
    items: Vec<ScriptFilterItem>,
}

/// Container for script filter results with default icon
#[derive(Debug, Clone)]
struct ScriptFilterResult {
    items: Vec<ScriptFilterItem>,
    default_icon: Option<String>,
}

/// A file or directory entry for the file browser addon
#[derive(Debug, Clone)]
struct FileBrowserEntry {
    name: String,
    full_path: String,
    is_dir: bool,
}

/// Active web search state when a web search keyword is matched
#[derive(Debug, Clone)]
struct WebSearchActiveState {
    name: String,
    query: String,
    url_template: String,
    icon: Option<String>,
}

const DEFAULT_CURRENCIES: &[&str] = &["USD", "EUR", "GBP"];

/// Cached exchange rate with timestamp for TTL
#[derive(Debug, Clone)]
struct CachedRate {
    rate: f64,
    timestamp: Instant,
}

impl CachedRate {
    const TTL: Duration = Duration::from_secs(3600); // 1 hour

    fn new(rate: f64) -> Self {
        Self {
            rate,
            timestamp: Instant::now(),
        }
    }

    fn is_valid(&self) -> bool {
        self.timestamp.elapsed() < Self::TTL
    }
}

/// Frankfurter API response
#[derive(Debug, Deserialize)]
struct FrankfurterResponse {
    rates: HashMap<String, f64>,
}

// Supported currencies from Frankfurter API
const SUPPORTED_CURRENCIES: &[&str] = &[
    "EUR", "USD", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "HKD", "NZD", "SEK", "KRW", "SGD",
    "NOK", "MXN", "INR", "RUB", "ZAR", "TRY", "BRL", "TWD", "DKK", "PLN", "THB", "IDR", "HUF",
    "CZK", "ILS", "CLP", "PHP", "AED", "COP", "SAR", "MYR", "RON", "BGN", "ISK", "HRK",
];

// Pattern: "10 to EUR", "10 EUR to GBP", "10EUR to GBP" (trigger prefix stripped before matching)
// Captures: amount, optional source currency, target currency
static PATTERN_CURRENCY_CONVERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(\d+(?:\.\d+)?)\s*([A-Z]{3})?\s*(?:to|in)\s+([A-Z]{3})$").unwrap()
});

// Pattern with word currencies: "10 euros to dollars" (trigger prefix stripped before matching)
static PATTERN_CURRENCY_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(\d+(?:\.\d+)?)\s*(dollars?|euros?|pounds?|yen|yuan)?\s*(?:to|in)\s+(dollars?|euros?|pounds?|yen|yuan)$").unwrap()
});

// Pattern: "10" or "10 EUR" (simple syntax without "to/in", trigger prefix stripped before matching)
static PATTERN_SIMPLE_CURRENCY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(\d+(?:\.\d+)?)\s*([A-Z]{3})?$").unwrap());

// ANSI SGR escape sequence pattern
static ANSI_SGR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[([0-9;]*)m").unwrap());

fn ansi_color(code: u8) -> Option<iced::Color> {
    match code {
        30 => Some(iced::Color::from_rgb(0.0, 0.0, 0.0)),
        31 => Some(iced::Color::from_rgb(0.8, 0.2, 0.2)),
        32 => Some(iced::Color::from_rgb(0.2, 0.8, 0.2)),
        33 => Some(iced::Color::from_rgb(0.8, 0.8, 0.2)),
        34 => Some(iced::Color::from_rgb(0.3, 0.3, 0.9)),
        35 => Some(iced::Color::from_rgb(0.8, 0.2, 0.8)),
        36 => Some(iced::Color::from_rgb(0.2, 0.8, 0.8)),
        37 => Some(iced::Color::from_rgb(0.9, 0.9, 0.9)),
        90 => Some(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        91 => Some(iced::Color::from_rgb(1.0, 0.3, 0.3)),
        92 => Some(iced::Color::from_rgb(0.3, 1.0, 0.3)),
        93 => Some(iced::Color::from_rgb(1.0, 1.0, 0.3)),
        94 => Some(iced::Color::from_rgb(0.5, 0.5, 1.0)),
        95 => Some(iced::Color::from_rgb(1.0, 0.3, 1.0)),
        96 => Some(iced::Color::from_rgb(0.3, 1.0, 1.0)),
        97 => Some(iced::Color::from_rgb(1.0, 1.0, 1.0)),
        _ => None,
    }
}

fn ansi_to_spans<'a>(
    s: &str,
    font_size: f32,
    default_color: iced::Color,
) -> Vec<iced::widget::text::Span<'a, (), iced::Font>> {
    let mut spans = Vec::new();
    let mut fg = default_color;
    let mut bold = false;
    let mut underline = false;
    let mut last_end = 0;

    for cap in ANSI_SGR_RE.captures_iter(s) {
        let m = cap.get(0).unwrap();
        let before = &s[last_end..m.start()];
        if !before.is_empty() {
            let mut sp = span(before.to_owned()).size(font_size).color(fg);
            if underline {
                sp = sp.underline(true);
            }
            if bold {
                sp = sp.font(iced::Font {
                    weight: iced::font::Weight::Bold,
                    ..iced::Font::default()
                });
            }
            spans.push(sp);
        }
        last_end = m.end();

        let params = &cap[1];
        if params.is_empty() {
            // \x1b[m is equivalent to reset
            fg = default_color;
            bold = false;
            underline = false;
            continue;
        }
        for part in params.split(';') {
            if let Ok(code) = part.parse::<u8>() {
                match code {
                    0 => {
                        fg = default_color;
                        bold = false;
                        underline = false;
                    }
                    1 => bold = true,
                    4 => underline = true,
                    22 => bold = false,
                    24 => underline = false,
                    30..=37 | 90..=97 => {
                        if let Some(c) = ansi_color(code) {
                            fg = c;
                        }
                    }
                    _ => {} // ignore unsupported codes
                }
            }
        }
    }

    // Remaining text after the last escape sequence
    let tail = &s[last_end..];
    if !tail.is_empty() || spans.is_empty() {
        let mut sp = span(tail.to_owned()).size(font_size).color(fg);
        if underline {
            sp = sp.underline(true);
        }
        if bold {
            sp = sp.font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::default()
            });
        }
        spans.push(sp);
    }

    spans
}

fn is_currency_help_query(query: &str, trigger: &str) -> bool {
    let trimmed = query.trim();
    trimmed == trigger || trimmed == format!("{} ", trigger)
}

fn word_to_currency(word: &str) -> Option<&'static str> {
    match word.to_lowercase().as_str() {
        "dollar" | "dollars" => Some("USD"),
        "euro" | "euros" => Some("EUR"),
        "pound" | "pounds" => Some("GBP"),
        "yen" => Some("JPY"),
        "yuan" => Some("CNY"),
        _ => None,
    }
}

fn is_valid_currency(code: &str) -> bool {
    SUPPORTED_CURRENCIES.contains(&code.to_uppercase().as_str())
}

fn try_parse_currency_conversion(
    query: &str,
    default_currency: &str,
    trigger: &str,
) -> Option<CurrencyConversionRequest> {
    let trimmed = query.trim();

    // Must start with trigger
    if !trimmed.starts_with(trigger) {
        return None;
    }

    // Strip the trigger prefix
    let after_trigger = &trimmed[trigger.len()..];

    // Try pattern: "10 to EUR" or "10 EUR to GBP" or "10EUR to GBP"
    if let Some(caps) = PATTERN_CURRENCY_CONVERSION.captures(after_trigger) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;
        let from = caps
            .get(2)
            .map(|m| m.as_str().to_uppercase())
            .unwrap_or_else(|| default_currency.to_string());
        let to = caps.get(3)?.as_str().to_uppercase();

        if is_valid_currency(&from) && is_valid_currency(&to) && from != to {
            return Some(CurrencyConversionRequest {
                amount,
                from_currency: from,
                to_currency: to,
            });
        }
    }

    // Try word pattern: "10 euros to dollars"
    if let Some(caps) = PATTERN_CURRENCY_WORDS.captures(after_trigger) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;
        let from = caps
            .get(2)
            .and_then(|m| word_to_currency(m.as_str()))
            .unwrap_or(default_currency);
        let to = word_to_currency(caps.get(3)?.as_str())?;

        if from != to {
            return Some(CurrencyConversionRequest {
                amount,
                from_currency: from.to_string(),
                to_currency: to.to_string(),
            });
        }
    }

    None
}

/// Parse multi-currency conversion request (simple syntax without "to/in")
/// - "$10" → convert from default currency to all configured currencies
/// - "$10 EUR" → convert from EUR to all other configured currencies
fn try_parse_multi_currency_conversion(
    query: &str,
    config_currencies: &[String],
    default_currency: &str,
    trigger: &str,
) -> Option<MultiCurrencyRequest> {
    let trimmed = query.trim();

    // Must start with trigger
    if !trimmed.starts_with(trigger) {
        return None;
    }

    // Strip the trigger prefix
    let after_trigger = &trimmed[trigger.len()..];

    // Skip if it matches explicit "to/in" syntax (let existing parser handle it)
    if PATTERN_CURRENCY_CONVERSION.is_match(after_trigger)
        || PATTERN_CURRENCY_WORDS.is_match(after_trigger)
    {
        return None;
    }

    // Try simple pattern: "10" or "10 EUR"
    if let Some(caps) = PATTERN_SIMPLE_CURRENCY.captures(after_trigger) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;

        // Get configured currencies or use defaults
        let currencies: Vec<String> = if config_currencies.is_empty() {
            DEFAULT_CURRENCIES.iter().map(|s| s.to_string()).collect()
        } else {
            config_currencies.to_vec()
        };

        if currencies.len() < 2 {
            return None;
        }

        // Determine source currency
        let from_currency = if let Some(m) = caps.get(2) {
            let code = m.as_str().to_uppercase();
            if !is_valid_currency(&code) {
                return None;
            }
            code
        } else {
            // No currency specified, use default_currency
            default_currency.to_string()
        };

        // Target currencies are all others in the config
        let to_currencies: Vec<String> = currencies
            .iter()
            .filter(|c| c.to_uppercase() != from_currency.to_uppercase())
            .cloned()
            .collect();

        if to_currencies.is_empty() {
            return None;
        }

        return Some(MultiCurrencyRequest {
            amount,
            from_currency,
            to_currencies,
        });
    }

    None
}

fn fetch_exchange_rate(request: CurrencyConversionRequest) -> Task<Message> {
    let request_for_result = request.clone();
    Task::perform(
        async move { fetch_rate_blocking(&request) },
        move |result| Message::CurrencyConversionResult(request_for_result, result),
    )
}

fn fetch_rate_blocking(request: &CurrencyConversionRequest) -> Result<CurrencyResult, String> {
    let url = format!(
        "https://api.frankfurter.dev/v1/latest?base={}&symbols={}",
        request.from_currency, request.to_currency
    );

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build();
    let agent: ureq::Agent = config.into();

    let response: FrankfurterResponse = agent
        .get(&url)
        .call()
        .map_err(|e| format!("Network error: {}", e))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("Parse error: {}", e))?;

    let rate = response
        .rates
        .get(&request.to_currency)
        .copied()
        .ok_or_else(|| "Rate not found".to_string())?;

    let converted_amount = request.amount * rate;

    Ok(CurrencyResult {
        request: request.clone(),
        converted_amount,
        rate,
    })
}

fn fetch_multi_exchange_rates(request: MultiCurrencyRequest) -> Task<Message> {
    let request_for_result = request.clone();
    Task::perform(
        async move { fetch_multi_rates_blocking(&request) },
        move |result| Message::MultiCurrencyConversionResult(request_for_result, result),
    )
}

fn fetch_multi_rates_blocking(
    request: &MultiCurrencyRequest,
) -> Result<MultiCurrencyResult, String> {
    let symbols = request.to_currencies.join(",");
    let url = format!(
        "https://api.frankfurter.dev/v1/latest?base={}&symbols={}",
        request.from_currency, symbols
    );

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build();
    let agent: ureq::Agent = config.into();

    let response: FrankfurterResponse = agent
        .get(&url)
        .call()
        .map_err(|e| format!("Network error: {}", e))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("Parse error: {}", e))?;

    let conversions: Vec<CurrencyConversion> = request
        .to_currencies
        .iter()
        .filter_map(|to_currency| {
            response
                .rates
                .get(to_currency)
                .map(|&rate| CurrencyConversion {
                    to_currency: to_currency.clone(),
                    converted_amount: request.amount * rate,
                    rate,
                })
        })
        .collect();

    if conversions.is_empty() {
        return Err("No rates found".to_string());
    }

    Ok(MultiCurrencyResult {
        amount: request.amount,
        from_currency: request.from_currency.clone(),
        conversions,
    })
}

fn execute_script_filter(
    command: String,
    args: Vec<String>,
    query: String,
    generation: u64,
    default_icon: Option<String>,
) -> Task<Message> {
    Task::perform(
        async move {
            let output = Command::new(&command)
                .args(&args)
                .arg(&query)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output();

            match output {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    match serde_json::from_str::<ScriptFilterResponse>(&stdout) {
                        Ok(response) => Ok(ScriptFilterResult {
                            items: response.items,
                            default_icon,
                        }),
                        Err(e) => {
                            eprintln!("Script filter: invalid JSON from {}: {}", command, e);
                            Err(format!("Invalid JSON: {}", e))
                        }
                    }
                }
                Ok(output) => {
                    eprintln!(
                        "Script filter: {} exited with status {}",
                        command, output.status
                    );
                    Err(format!("Script exited with status {}", output.status))
                }
                Err(e) => {
                    eprintln!("Script filter: failed to execute {}: {}", command, e);
                    Err(format!("Failed to execute: {}", e))
                }
            }
        },
        move |result| Message::ScriptFilterResult(generation, result),
    )
}

/// Read a directory and return entries, with dirs sorted first then files, alphabetically.
fn read_directory(path: &str, show_hidden: bool) -> Vec<FileBrowserEntry> {
    let dir_path = Path::new(path);
    let entries = match fs::read_dir(dir_path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let full_path = entry.path().to_string_lossy().to_string();
        let fb_entry = FileBrowserEntry {
            name,
            full_path,
            is_dir,
        };
        if is_dir {
            dirs.push(fb_entry);
        } else {
            files.push(fb_entry);
        }
    }

    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    dirs.append(&mut files);
    dirs
}

/// Guess a mimetype icon name from a file extension.
fn mimetype_icon_name(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    // Borrow ext as a &str for matching
    match ext.as_str() {
        "txt" | "md" | "log" | "cfg" | "conf" | "ini" | "toml" | "yaml" | "yml" | "json"
        | "xml" | "csv" | "rst" | "tex" => "text-x-generic",
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "h" | "java" | "rb" | "sh" | "bash"
        | "zsh" | "fish" | "pl" | "lua" | "hs" | "ml" | "ex" | "exs" | "clj" | "scala" | "kt"
        | "swift" | "r" | "sql" | "html" | "css" | "scss" | "less" | "jsx" | "tsx" | "vue"
        | "svelte" => "text-x-script",
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp" | "ico" | "tiff" | "tif" => {
            "image-x-generic"
        }
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "wma" | "m4a" | "opus" => "audio-x-generic",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" => "video-x-generic",
        "pdf" => "application-pdf",
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" => "package-x-generic",
        "deb" | "rpm" => "package-x-generic",
        "iso" | "img" => "media-optical",
        "doc" | "docx" | "odt" | "rtf" => "x-office-document",
        "xls" | "xlsx" | "ods" => "x-office-spreadsheet",
        "ppt" | "pptx" | "odp" => "x-office-presentation",
        _ => "text-x-generic",
    }
}

/// The main application state
struct LauncherApp {
    configs: Vec<RaffiConfig>,
    filtered_configs: Vec<usize>,
    search_query: String,
    selected_index: usize,
    selected_item: SharedSelection,
    icon_map: HashMap<String, String>,
    mru_map: HashMap<String, u32>,
    search_input_id: TextInputId,
    scrollable_id: ScrollableId,
    items_container_id: ContainerId,
    view_generation: u64,
    addons: AddonsConfig,
    calculator_result: Option<CalculatorResult>,
    currency_result: Option<CurrencyResult>,
    currency_loading: bool,
    currency_error: Option<String>,
    currency_cache: HashMap<String, CachedRate>,
    pending_currency_request: Option<CurrencyConversionRequest>,
    currency_help: bool,
    // Multi-currency conversion state
    multi_currency_result: Option<MultiCurrencyResult>,
    multi_currency_loading: bool,
    pending_multi_currency_request: Option<MultiCurrencyRequest>,
    // Script filter state
    script_filter_results: Option<ScriptFilterResult>,
    script_filter_loading: bool,
    script_filter_loading_name: Option<String>,
    script_filter_generation: u64,
    script_filter_action: Option<String>,
    script_filter_secondary_action: Option<String>,
    // Web search state
    web_search_active: Option<WebSearchActiveState>,
    // File browser state
    file_browser_entries: Vec<FileBrowserEntry>,
    file_browser_all_entries: Vec<FileBrowserEntry>,
    file_browser_active: bool,
    file_browser_show_hidden: bool,
    file_browser_current_dir: String,
    file_browser_error: Option<String>,
    current_modifiers: iced::keyboard::Modifiers,
    theme: ThemeColors,
    // Command history state
    history: Vec<String>,
    history_index: Option<usize>,
    history_saved_query: String,
    history_search_in_progress: bool,
    max_history: u32,
    show_hints: bool,
}

#[derive(Debug, Clone)]
enum Message {
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
    MultiCurrencyResultCopied(usize), // index of conversion to copy
    ScriptFilterResult(u64, std::result::Result<ScriptFilterResult, String>),
    ScriptFilterItemSelected(usize),
    WebSearchSelected,
    FileBrowserItemSelected(usize),
    FileBrowserTabComplete,
    FileBrowserToggleHidden,
    ModifiersChanged(iced::keyboard::Modifiers),
    HistoryPrevious,
    HistoryNext,
    ToggleHints,
}

impl LauncherApp {
    fn new(
        mut configs: Vec<RaffiConfig>,
        addons: AddonsConfig,
        no_icons: bool,
        selected_item: SharedSelection,
        initial_query: Option<String>,
        theme: ThemeColors,
        max_history: u32,
    ) -> (Self, Task<Message>) {
        let icon_map = if no_icons {
            HashMap::new()
        } else {
            read_icon_map().unwrap_or_default()
        };

        let mru_map = load_mru_map();
        configs.sort_by_key(|config| {
            let description = config
                .description
                .as_deref()
                .unwrap_or_else(|| config.binary.as_deref().unwrap_or(""));
            std::cmp::Reverse(mru_map.get(description).copied().unwrap_or(0))
        });

        let initial_query = initial_query.unwrap_or_default();
        let filtered_configs: Vec<usize> = (0..configs.len()).collect();
        let search_input_id = TextInputId::unique();
        let scrollable_id = ScrollableId::unique();
        let items_container_id = ContainerId::unique();
        let file_browser_show_hidden = addons.file_browser.show_hidden.unwrap_or(false);

        (
            LauncherApp {
                configs,
                filtered_configs,
                search_query: initial_query.clone(),
                selected_index: 0,
                selected_item,
                icon_map,
                mru_map,
                search_input_id: search_input_id.clone(),
                scrollable_id,
                items_container_id,
                view_generation: 0,
                addons,
                calculator_result: None,
                currency_result: None,
                currency_loading: false,
                currency_error: None,
                currency_cache: HashMap::new(),
                pending_currency_request: None,
                currency_help: false,
                multi_currency_result: None,
                multi_currency_loading: false,
                pending_multi_currency_request: None,
                script_filter_results: None,
                script_filter_loading: false,
                script_filter_loading_name: None,
                script_filter_generation: 0,
                script_filter_action: None,
                script_filter_secondary_action: None,
                web_search_active: None,
                file_browser_entries: Vec::new(),
                file_browser_all_entries: Vec::new(),
                file_browser_active: false,
                file_browser_show_hidden,
                file_browser_current_dir: String::new(),
                file_browser_error: None,
                current_modifiers: iced::keyboard::Modifiers::empty(),
                theme,
                history: load_history(max_history),
                history_index: None,
                history_saved_query: String::new(),
                history_search_in_progress: false,
                max_history,
                show_hints: false,
            },
            if initial_query.is_empty() {
                focus(search_input_id)
            } else {
                focus(search_input_id).chain(Task::done(Message::SearchChanged(initial_query)))
            },
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => {
                if self.current_modifiers.alt() && !self.history_search_in_progress {
                    return Task::none();
                }
                self.history_search_in_progress = false;
                self.search_query = query.clone();
                self.filter_items(&query);
                self.calculator_result = if self.addons.calculator.enabled {
                    try_evaluate_math(&query)
                } else {
                    None
                };
                self.selected_index = 0;
                // Reset history navigation when the user types manually
                // (but not when we programmatically set the query via history nav)
                // We detect this by checking if the query matches the current history entry
                if let Some(idx) = self.history_index {
                    if self.history.get(idx).map(|h| h.as_str()) != Some(&query) {
                        self.history_index = None;
                    }
                }
                // Regenerate IDs to force complete view refresh
                self.scrollable_id = ScrollableId::unique();
                self.items_container_id = ContainerId::unique();
                self.view_generation = self.view_generation.wrapping_add(1);

                let mut tasks: Vec<Task<Message>> = Vec::new();

                // Check for file browser trigger (/ or ~)
                let trimmed = query.trim();
                let is_file_browser_query = self.addons.file_browser.enabled
                    && (trimmed.starts_with('/') || trimmed == "~" || trimmed.starts_with("~/"));
                if is_file_browser_query {
                    self.file_browser_active = true;
                    self.file_browser_error = None;

                    // Expand ~ to home directory
                    let expanded = if trimmed == "~" {
                        format!("{}/", std::env::var("HOME").unwrap_or_default())
                    } else if let Some(rest) = trimmed.strip_prefix("~/") {
                        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
                    } else {
                        trimmed.to_string()
                    };

                    // Split into directory part and filter part at the last /
                    let (dir_path, filter_text) = if expanded.ends_with('/') {
                        (expanded.as_str(), "")
                    } else if let Some(last_slash) = expanded.rfind('/') {
                        (&expanded[..=last_slash], &expanded[last_slash + 1..])
                    } else {
                        (expanded.as_str(), "")
                    };

                    // Only re-read directory if the directory changed
                    if dir_path != self.file_browser_current_dir {
                        self.file_browser_current_dir = dir_path.to_string();
                        let all_entries = read_directory(dir_path, self.file_browser_show_hidden);
                        if all_entries.is_empty() && !Path::new(dir_path).is_dir() {
                            self.file_browser_error =
                                Some(format!("Cannot read directory: {}", dir_path));
                        }
                        self.file_browser_all_entries = all_entries;
                    }

                    // Apply fuzzy filter on filenames
                    if filter_text.is_empty() {
                        self.file_browser_entries = self.file_browser_all_entries.clone();
                    } else {
                        let matcher = SkimMatcherV2::default();
                        let mut scored: Vec<(usize, i64)> = self
                            .file_browser_all_entries
                            .iter()
                            .enumerate()
                            .filter_map(|(i, entry)| {
                                matcher
                                    .fuzzy_match(&entry.name, filter_text)
                                    .map(|score| (i, score))
                            })
                            .collect();
                        scored.sort_by(|a, b| b.1.cmp(&a.1));
                        self.file_browser_entries = scored
                            .into_iter()
                            .map(|(i, _)| self.file_browser_all_entries[i].clone())
                            .collect();
                    }

                    // Clear regular items and other addon states
                    self.filtered_configs.clear();
                    self.script_filter_results = None;
                    self.script_filter_loading = false;
                    self.script_filter_loading_name = None;
                    self.script_filter_action = None;
                    self.script_filter_secondary_action = None;
                    self.web_search_active = None;
                    self.calculator_result = None;
                    self.currency_result = None;
                    self.currency_loading = false;
                    self.currency_error = None;
                    self.pending_currency_request = None;
                    self.multi_currency_result = None;
                    self.multi_currency_loading = false;
                    self.pending_multi_currency_request = None;
                    self.currency_help = false;

                    return Task::batch(tasks);
                }

                // Clear file browser state when not a file browser query
                self.file_browser_active = false;
                self.file_browser_entries.clear();
                self.file_browser_all_entries.clear();
                self.file_browser_current_dir.clear();
                self.file_browser_error = None;

                // Check for script filter keyword match
                let mut script_filter_matched = false;
                for sf_config in &self.addons.script_filters {
                    let keyword = &sf_config.keyword;
                    // Match "keyword" exactly or "keyword " prefix
                    if trimmed == keyword.as_str() || trimmed.starts_with(&format!("{} ", keyword))
                    {
                        script_filter_matched = true;
                        let sf_query = if trimmed.len() > keyword.len() {
                            trimmed[keyword.len()..].trim_start().to_string()
                        } else {
                            String::new()
                        };

                        // Clear regular config items when script filter is active
                        self.filtered_configs.clear();

                        self.script_filter_generation =
                            self.script_filter_generation.wrapping_add(1);
                        self.script_filter_loading = true;
                        self.script_filter_loading_name = Some(sf_config.name.clone());
                        self.script_filter_action = sf_config.action.clone();
                        self.script_filter_secondary_action = sf_config.secondary_action.clone();
                        self.script_filter_results = None;

                        tasks.push(execute_script_filter(
                            sf_config.command.clone(),
                            sf_config.args.clone(),
                            sf_query,
                            self.script_filter_generation,
                            sf_config.icon.clone(),
                        ));
                        break;
                    }
                }

                if !script_filter_matched {
                    // Clear script filter state
                    self.script_filter_results = None;
                    self.script_filter_loading = false;
                    self.script_filter_loading_name = None;
                    self.script_filter_action = None;
                    self.script_filter_secondary_action = None;

                    // Check for web search keyword match
                    let mut web_search_matched = false;
                    for ws_config in &self.addons.web_searches {
                        let keyword = &ws_config.keyword;
                        if trimmed == keyword.as_str()
                            || trimmed.starts_with(&format!("{} ", keyword))
                        {
                            web_search_matched = true;
                            let ws_query = if trimmed.len() > keyword.len() {
                                trimmed[keyword.len()..].trim_start().to_string()
                            } else {
                                String::new()
                            };

                            // Clear regular config items when web search is active
                            self.filtered_configs.clear();

                            self.web_search_active = Some(WebSearchActiveState {
                                name: ws_config.name.clone(),
                                query: ws_query,
                                url_template: ws_config.url.clone(),
                                icon: ws_config.icon.clone(),
                            });
                            break;
                        }
                    }

                    if !web_search_matched {
                        self.web_search_active = None;
                    }
                } else {
                    self.web_search_active = None;
                }

                // Determine trigger from config
                let trigger = self.addons.currency.trigger.as_deref().unwrap_or("$");

                // Check for currency help (just trigger) - only if currency addon is enabled
                self.currency_help =
                    self.addons.currency.enabled && is_currency_help_query(&query, trigger);

                if self.currency_help {
                    self.currency_result = None;
                    self.currency_loading = false;
                    self.currency_error = None;
                    self.pending_currency_request = None;
                    self.multi_currency_result = None;
                    self.multi_currency_loading = false;
                    self.pending_multi_currency_request = None;
                    return Task::batch(tasks);
                }

                // Check for currency conversion request - only if currency addon is enabled
                if self.addons.currency.enabled {
                    // Determine default currency from config
                    let default_currency = self
                        .addons
                        .currency
                        .default_currency
                        .as_deref()
                        .unwrap_or("USD");

                    // First try single-currency conversion (explicit "to/in" syntax)
                    if let Some(currency_request) =
                        try_parse_currency_conversion(&query, default_currency, trigger)
                    {
                        // Clear multi-currency state
                        self.multi_currency_result = None;
                        self.multi_currency_loading = false;
                        self.pending_multi_currency_request = None;

                        let cache_key = format!(
                            "{}_{}",
                            currency_request.from_currency, currency_request.to_currency
                        );

                        // Check cache first
                        if let Some(cached) = self.currency_cache.get(&cache_key) {
                            if cached.is_valid() {
                                let converted_amount = currency_request.amount * cached.rate;
                                self.currency_result = Some(CurrencyResult {
                                    request: currency_request,
                                    converted_amount,
                                    rate: cached.rate,
                                });
                                self.currency_loading = false;
                                self.currency_error = None;
                                self.pending_currency_request = None;
                                return Task::batch(tasks);
                            }
                        }

                        // Need to fetch from API
                        self.currency_loading = true;
                        self.currency_result = None;
                        self.currency_error = None;
                        self.pending_currency_request = Some(currency_request.clone());
                        tasks.push(fetch_exchange_rate(currency_request));
                        return Task::batch(tasks);
                    }

                    // Try multi-currency conversion (simple syntax like "$10" or "$10 EUR")
                    let config_currencies =
                        self.addons.currency.currencies.clone().unwrap_or_default();
                    if let Some(multi_request) = try_parse_multi_currency_conversion(
                        &query,
                        &config_currencies,
                        default_currency,
                        trigger,
                    ) {
                        // Clear single-currency state
                        self.currency_result = None;
                        self.currency_loading = false;
                        self.currency_error = None;
                        self.pending_currency_request = None;

                        // Check if all rates are cached
                        let mut all_cached = true;
                        let mut conversions = Vec::new();
                        for to_currency in &multi_request.to_currencies {
                            let cache_key =
                                format!("{}_{}", multi_request.from_currency, to_currency);
                            if let Some(cached) = self.currency_cache.get(&cache_key) {
                                if cached.is_valid() {
                                    conversions.push(CurrencyConversion {
                                        to_currency: to_currency.clone(),
                                        converted_amount: multi_request.amount * cached.rate,
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
                            self.multi_currency_result = Some(MultiCurrencyResult {
                                amount: multi_request.amount,
                                from_currency: multi_request.from_currency.clone(),
                                conversions,
                            });
                            self.multi_currency_loading = false;
                            self.pending_multi_currency_request = None;
                            return Task::batch(tasks);
                        }

                        // Need to fetch from API
                        self.multi_currency_loading = true;
                        self.multi_currency_result = None;
                        self.pending_multi_currency_request = Some(multi_request.clone());
                        tasks.push(fetch_multi_exchange_rates(multi_request));
                        return Task::batch(tasks);
                    }

                    // Clear all currency state if no conversion request
                    self.currency_result = None;
                    self.currency_loading = false;
                    self.currency_error = None;
                    self.pending_currency_request = None;
                    self.multi_currency_result = None;
                    self.multi_currency_loading = false;
                    self.pending_multi_currency_request = None;
                } else {
                    // Currency addon disabled - clear any currency state
                    self.currency_result = None;
                    self.currency_loading = false;
                    self.currency_error = None;
                    self.pending_currency_request = None;
                    self.multi_currency_result = None;
                    self.multi_currency_loading = false;
                    self.pending_multi_currency_request = None;
                }

                Task::batch(tasks)
            }
            Message::MoveUp => {
                let total = self.total_items();
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                } else if total > 0 {
                    // Wrap around to bottom
                    self.selected_index = total - 1;
                }

                if total > 1 {
                    let offset = self.selected_index as f32 / (total - 1) as f32;
                    snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::MoveDown => {
                let total = self.total_items();
                // Move selection down
                if self.selected_index < total.saturating_sub(1) {
                    self.selected_index += 1;
                } else {
                    // Wrap around to top
                    self.selected_index = 0;
                }

                if total > 1 {
                    let offset = self.selected_index as f32 / (total - 1) as f32;
                    snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::Submit => {
                // Track position offsets for special items
                let mut current_idx = 0;

                // Check if script filter loading/results are selected
                if self.script_filter_loading {
                    if self.selected_index == current_idx {
                        return Task::none();
                    }
                    current_idx += 1;
                } else if let Some(ref sf_result) = self.script_filter_results {
                    let num_items = sf_result.items.len();
                    if self.selected_index >= current_idx
                        && self.selected_index < current_idx + num_items
                    {
                        let item_idx = self.selected_index - current_idx;
                        return self.update(Message::ScriptFilterItemSelected(item_idx));
                    }
                    current_idx += num_items;
                }

                // Check if file browser entries are selected
                if self.file_browser_active {
                    let num_entries = self.file_browser_entries.len();
                    if num_entries > 0
                        && self.selected_index >= current_idx
                        && self.selected_index < current_idx + num_entries
                    {
                        let entry_idx = self.selected_index - current_idx;
                        return self.update(Message::FileBrowserItemSelected(entry_idx));
                    }
                    current_idx += num_entries;
                }

                // Check if web search row is selected
                if self.web_search_active.is_some() {
                    if self.selected_index == current_idx {
                        return self.update(Message::WebSearchSelected);
                    }
                    current_idx += 1;
                }

                // Check if single currency loading/result is selected
                if self.currency_loading {
                    if self.selected_index == current_idx {
                        return Task::none();
                    }
                    current_idx += 1;
                } else if self.currency_result.is_some() {
                    if self.selected_index == current_idx {
                        return self.update(Message::CurrencyResultCopied);
                    }
                    current_idx += 1;
                }

                // Check if multi-currency loading/results are selected
                if self.multi_currency_loading {
                    if self.selected_index == current_idx {
                        return Task::none();
                    }
                    current_idx += 1;
                } else if let Some(ref multi_result) = self.multi_currency_result {
                    let num_conversions = multi_result.conversions.len();
                    if self.selected_index >= current_idx
                        && self.selected_index < current_idx + num_conversions
                    {
                        let conversion_idx = self.selected_index - current_idx;
                        return self.update(Message::MultiCurrencyResultCopied(conversion_idx));
                    }
                    current_idx += num_conversions;
                }

                // Check if calculator is selected
                if self.calculator_result.is_some() {
                    if self.selected_index == current_idx {
                        return self.update(Message::CalculatorSelected);
                    }
                    current_idx += 1;
                }

                // Adjust index for config lookup
                let config_index = self.selected_index.saturating_sub(current_idx);

                if let Some(&config_idx) = self.filtered_configs.get(config_index) {
                    let config = &self.configs[config_idx];
                    let description = config
                        .description
                        .clone()
                        .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
                    if let Ok(mut selected) = self.selected_item.lock() {
                        *selected = Some(description.clone());
                    }
                    let count = self.mru_map.entry(description).or_insert(0);
                    *count += 1;
                    save_mru_map(&self.mru_map);
                    self.save_query_to_history();
                }
                iced::exit()
            }
            Message::Cancel => {
                // Don't set selection, just close
                iced::exit()
            }
            Message::ItemClicked(idx) => {
                // Set the clicked item as selected and submit
                self.selected_index = idx;

                // Track position offsets for special items
                let mut current_idx = 0;

                // Check if script filter loading/results are clicked
                if self.script_filter_loading {
                    if idx == current_idx {
                        return Task::none();
                    }
                    current_idx += 1;
                } else if let Some(ref sf_result) = self.script_filter_results {
                    let num_items = sf_result.items.len();
                    if idx >= current_idx && idx < current_idx + num_items {
                        let item_idx = idx - current_idx;
                        return self.update(Message::ScriptFilterItemSelected(item_idx));
                    }
                    current_idx += num_items;
                }

                // Check if file browser entries are clicked
                if self.file_browser_active {
                    let num_entries = self.file_browser_entries.len();
                    if num_entries > 0 && idx >= current_idx && idx < current_idx + num_entries {
                        let entry_idx = idx - current_idx;
                        return self.update(Message::FileBrowserItemSelected(entry_idx));
                    }
                    current_idx += num_entries;
                }

                // Check if web search row is clicked
                if self.web_search_active.is_some() {
                    if idx == current_idx {
                        return self.update(Message::WebSearchSelected);
                    }
                    current_idx += 1;
                }

                // Check if single currency loading/result is clicked
                if self.currency_loading {
                    if idx == current_idx {
                        return Task::none();
                    }
                    current_idx += 1;
                } else if self.currency_result.is_some() {
                    if idx == current_idx {
                        return self.update(Message::CurrencyResultCopied);
                    }
                    current_idx += 1;
                }

                // Check if multi-currency loading/results are clicked
                if self.multi_currency_loading {
                    if idx == current_idx {
                        return Task::none();
                    }
                    current_idx += 1;
                } else if let Some(ref multi_result) = self.multi_currency_result {
                    let num_conversions = multi_result.conversions.len();
                    if idx >= current_idx && idx < current_idx + num_conversions {
                        let conversion_idx = idx - current_idx;
                        return self.update(Message::MultiCurrencyResultCopied(conversion_idx));
                    }
                    current_idx += num_conversions;
                }

                // Check if calculator is clicked
                if self.calculator_result.is_some() {
                    if idx == current_idx {
                        return self.update(Message::CalculatorSelected);
                    }
                    current_idx += 1;
                }

                // Adjust index for config lookup
                let config_index = idx.saturating_sub(current_idx);

                if let Some(&config_idx) = self.filtered_configs.get(config_index) {
                    let config = &self.configs[config_idx];
                    let description = config
                        .description
                        .clone()
                        .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
                    if let Ok(mut selected) = self.selected_item.lock() {
                        *selected = Some(description.clone());
                    }
                    let count = self.mru_map.entry(description).or_insert(0);
                    *count += 1;
                    save_mru_map(&self.mru_map);
                    self.save_query_to_history();
                }
                iced::exit()
            }
            Message::CalculatorSelected => {
                if let Some(ref calc) = self.calculator_result {
                    // Format the result nicely (remove trailing zeros for whole numbers)
                    let result_str = if calc.result.fract() == 0.0 {
                        format!("{}", calc.result as i64)
                    } else {
                        format!("{}", calc.result)
                    };
                    // Copy result to clipboard using wl-copy
                    let _ = Command::new("wl-copy")
                        .arg(&result_str)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
                self.save_query_to_history();
                iced::exit()
            }
            Message::CurrencyConversionResult(request, result) => {
                if self.pending_currency_request.as_ref() != Some(&request) {
                    return Task::none();
                }

                self.currency_loading = false;
                match result {
                    Ok(currency_result) => {
                        // Cache the rate
                        let cache_key = format!(
                            "{}_{}",
                            currency_result.request.from_currency,
                            currency_result.request.to_currency
                        );
                        self.currency_cache
                            .insert(cache_key, CachedRate::new(currency_result.rate));

                        self.currency_result = Some(currency_result);
                        self.currency_error = None;
                    }
                    Err(err) => {
                        self.currency_result = None;
                        self.currency_error = Some(err);
                    }
                }
                self.pending_currency_request = None;
                Task::none()
            }
            Message::CurrencyResultCopied => {
                if let Some(ref currency) = self.currency_result {
                    // Format the result for clipboard
                    let result_str = format!("{:.2}", currency.converted_amount);
                    // Copy result to clipboard using wl-copy
                    let _ = Command::new("wl-copy")
                        .arg(&result_str)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
                self.save_query_to_history();
                iced::exit()
            }
            Message::MultiCurrencyConversionResult(request, result) => {
                if self.pending_multi_currency_request.as_ref() != Some(&request) {
                    return Task::none();
                }

                self.multi_currency_loading = false;
                match result {
                    Ok(multi_result) => {
                        // Cache all the rates
                        for conversion in &multi_result.conversions {
                            let cache_key = format!(
                                "{}_{}",
                                multi_result.from_currency, conversion.to_currency
                            );
                            self.currency_cache
                                .insert(cache_key, CachedRate::new(conversion.rate));
                        }

                        self.multi_currency_result = Some(multi_result);
                    }
                    Err(_err) => {
                        self.multi_currency_result = None;
                    }
                }
                self.pending_multi_currency_request = None;
                Task::none()
            }
            Message::MultiCurrencyResultCopied(idx) => {
                if let Some(ref multi_result) = self.multi_currency_result {
                    if let Some(conversion) = multi_result.conversions.get(idx) {
                        // Format the result for clipboard
                        let result_str = format!("{:.2}", conversion.converted_amount);
                        // Copy result to clipboard using wl-copy
                        let _ = Command::new("wl-copy")
                            .arg(&result_str)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                    }
                }
                self.save_query_to_history();
                iced::exit()
            }
            Message::ScriptFilterResult(generation, result) => {
                if generation != self.script_filter_generation {
                    return Task::none();
                }
                self.script_filter_loading = false;
                self.script_filter_loading_name = None;
                match result {
                    Ok(sf_result) => {
                        self.script_filter_results = Some(sf_result);
                    }
                    Err(_) => {
                        self.script_filter_results = None;
                    }
                }
                Task::none()
            }
            Message::ScriptFilterItemSelected(idx) => {
                if let Some(ref sf_result) = self.script_filter_results {
                    if let Some(item) = sf_result.items.get(idx) {
                        let value = item.arg.as_deref().unwrap_or(&item.title);
                        let action_tpl = if self.current_modifiers.alt() {
                            self.script_filter_secondary_action
                                .as_ref()
                                .or(self.script_filter_action.as_ref())
                        } else {
                            self.script_filter_action.as_ref()
                        };
                        if let Some(action_tpl) = action_tpl {
                            let cmd = action_tpl.replace("{value}", value);
                            let _ = Command::new("sh")
                                .arg("-c")
                                .arg(&cmd)
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn();
                        } else {
                            let _ = Command::new("wl-copy")
                                .arg(value)
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::null())
                                .spawn();
                        }
                    }
                }
                self.save_query_to_history();
                iced::exit()
            }
            Message::WebSearchSelected => {
                if let Some(ref ws) = self.web_search_active {
                    if !ws.query.is_empty() {
                        let _ = execute_web_search_url(&ws.url_template, &ws.query);
                    }
                }
                self.save_query_to_history();
                iced::exit()
            }
            Message::FileBrowserItemSelected(idx) => {
                if let Some(entry) = self.file_browser_entries.get(idx) {
                    if entry.is_dir {
                        // Navigate into directory: update search query to the dir path
                        let new_query = format!("{}/", entry.full_path);
                        self.search_query = new_query.clone();
                        return Task::done(Message::SearchChanged(new_query));
                    }
                    // File selected
                    if self.current_modifiers.alt() {
                        // Alt+Enter: copy path to clipboard
                        let _ = Command::new("wl-copy")
                            .arg(&entry.full_path)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                    } else {
                        // Enter: open with xdg-open
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
            Message::FileBrowserTabComplete => {
                if !self.file_browser_active {
                    return Task::none();
                }
                // Compute which file browser entry is selected
                let mut current_idx = 0;
                if self.script_filter_loading {
                    current_idx += 1;
                } else if let Some(ref sf_result) = self.script_filter_results {
                    current_idx += sf_result.items.len();
                }
                let entry_idx = self.selected_index.saturating_sub(current_idx);
                if let Some(entry) = self.file_browser_entries.get(entry_idx) {
                    let new_query = if entry.is_dir {
                        format!("{}/", entry.full_path)
                    } else {
                        entry.full_path.clone()
                    };
                    self.search_query = new_query.clone();
                    let id = self.search_input_id.clone();
                    return Task::done(Message::SearchChanged(new_query))
                        .chain(move_cursor_to_end(id));
                }
                Task::none()
            }
            Message::FileBrowserToggleHidden => {
                self.file_browser_show_hidden = !self.file_browser_show_hidden;
                if self.file_browser_active && !self.file_browser_current_dir.is_empty() {
                    // Re-read and re-filter
                    self.file_browser_all_entries = read_directory(
                        &self.file_browser_current_dir,
                        self.file_browser_show_hidden,
                    );
                    // Re-trigger search to apply filter
                    let query = self.search_query.clone();
                    return Task::done(Message::SearchChanged(query));
                }
                Task::none()
            }
            Message::ModifiersChanged(modifiers) => {
                self.current_modifiers = modifiers;
                Task::none()
            }
            Message::HistoryPrevious => {
                if self.history.is_empty() || self.max_history == 0 {
                    return Task::none();
                }
                match self.history_index {
                    None => {
                        // Start navigating: save current query, go to most recent entry
                        self.history_saved_query = self.search_query.clone();
                        self.history_index = Some(self.history.len() - 1);
                    }
                    Some(0) => {
                        // Already at oldest entry, do nothing
                        return Task::none();
                    }
                    Some(idx) => {
                        self.history_index = Some(idx - 1);
                    }
                }
                let query = self.history[self.history_index.unwrap()].clone();
                let id = self.search_input_id.clone();
                self.history_search_in_progress = true;
                self.update(Message::SearchChanged(query))
                    .chain(move_cursor_to_end(id))
            }
            Message::HistoryNext => {
                if self.max_history == 0 {
                    return Task::none();
                }
                match self.history_index {
                    None => {
                        // Not navigating history, do nothing
                        Task::none()
                    }
                    Some(idx) => {
                        if idx + 1 >= self.history.len() {
                            // Past the end: restore saved query
                            self.history_index = None;
                            let query = self.history_saved_query.clone();
                            let id = self.search_input_id.clone();
                            self.history_search_in_progress = true;
                            self.update(Message::SearchChanged(query))
                                .chain(move_cursor_to_end(id))
                        } else {
                            self.history_index = Some(idx + 1);
                            let query = self.history[idx + 1].clone();
                            let id = self.search_input_id.clone();
                            self.history_search_in_progress = true;
                            self.update(Message::SearchChanged(query))
                                .chain(move_cursor_to_end(id))
                        }
                    }
                }
            }
            Message::ToggleHints => {
                self.show_hints = !self.show_hints;
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let t = self.theme;
        // --- Search Input Styling ---
        let search_input = text_input("Type to search...", &self.search_query)
            .id(self.search_input_id.clone())
            .on_input(Message::SearchChanged)
            .on_submit(Message::Submit)
            .padding(16)
            .size(24)
            .style(move |_theme, status| {
                let border_color = if matches!(status, text_input::Status::Focused { .. }) {
                    t.accent
                } else {
                    t.border
                };

                text_input::Style {
                    background: iced::Background::Color(t.bg_input),
                    border: iced::Border {
                        radius: 12.0.into(),
                        width: 1.0,
                        color: border_color,
                    },
                    placeholder: t.text_muted,
                    value: t.text_main,
                    selection: t.accent,
                    icon: t.text_muted,
                }
            })
            .width(Length::Fill);

        // --- List Items ---
        let mut items_column = Column::new().spacing(6);

        // Track special items for index offset calculation
        let has_script_filter = self.script_filter_results.is_some() || self.script_filter_loading;
        let has_file_browser = self.file_browser_active;
        let has_web_search = self.web_search_active.is_some();
        let has_currency = self.currency_result.is_some()
            || self.currency_loading
            || self.multi_currency_result.is_some()
            || self.multi_currency_loading;
        let has_calculator = self.calculator_result.is_some();

        // Current display index for special items
        let mut special_item_idx = 0;

        // Add script filter loading/results
        if self.script_filter_loading {
            let loading_name = self
                .script_filter_loading_name
                .as_deref()
                .unwrap_or("script filter");
            let loading_text = format!("Loading {}...", loading_name);

            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(loading_text).size(20).color(t.text_muted));

            let is_selected = self.selected_index == special_item_idx;

            let loading_button = button(loading_row).padding(12).width(Length::Fill).style(
                move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                },
            );

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if let Some(ref sf_result) = self.script_filter_results {
            for item in &sf_result.items {
                let is_selected = self.selected_index == special_item_idx;

                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                // Try to resolve icon: item icon path, or fallback to default_icon via icon_map
                let icon_path = item
                    .icon
                    .as_ref()
                    .and_then(|i| i.path.clone())
                    .and_then(|p| {
                        let expanded = crate::expand_config_value(&p);
                        if Path::new(&expanded).exists() {
                            Some(expanded)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        sf_result
                            .default_icon
                            .as_ref()
                            .and_then(|name| self.icon_map.get(name).cloned())
                    });

                if let Some(icon_path_str) = icon_path {
                    let icon_path = PathBuf::from(&icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.to_lowercase() == "svg")
                            .unwrap_or(false);

                        let icon_content: Element<Message> = if is_svg {
                            svg(iced::widget::svg::Handle::from_path(&icon_path))
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        } else {
                            image(icon_path)
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        };

                        item_row = item_row.push(icon_content);
                    }
                }

                // Title + optional subtitle
                let mut text_col = Column::new();
                text_col = text_col.push(rich_text(ansi_to_spans(&item.title, 20.0, t.text_main)));
                if let Some(ref subtitle) = item.subtitle {
                    text_col =
                        text_col.push(rich_text(ansi_to_spans(subtitle, 14.0, t.text_muted)));
                }
                item_row = item_row.push(text_col.width(Length::Fill));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.text_main,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..t.accent_hover
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None,
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(item_button);
                special_item_idx += 1;
            }
        }

        // Add file browser entries
        if self.file_browser_active {
            if let Some(ref err) = self.file_browser_error {
                let err_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(err.clone()).size(20).color(t.text_muted));

                let err_button = button(err_row).padding(12).width(Length::Fill).style(
                    move |_theme, _status| button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                );

                items_column = items_column.push(err_button);
            }

            for (idx, entry) in self.file_browser_entries.iter().enumerate() {
                let is_selected = self.selected_index == special_item_idx;

                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                // Icon
                if !self.icon_map.is_empty() {
                    let icon_name = if entry.is_dir {
                        "folder"
                    } else {
                        mimetype_icon_name(&entry.full_path)
                    };
                    if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                        let icon_path = PathBuf::from(icon_path_str);
                        if icon_path.exists() {
                            let is_svg = icon_path
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .map(|ext| ext.to_lowercase() == "svg")
                                .unwrap_or(false);

                            let icon_content: Element<Message> = if is_svg {
                                svg(iced::widget::svg::Handle::from_path(&icon_path))
                                    .width(Length::Fixed(40.0))
                                    .height(Length::Fixed(40.0))
                                    .content_fit(iced::ContentFit::Contain)
                                    .into()
                            } else {
                                image(icon_path)
                                    .width(Length::Fixed(40.0))
                                    .height(Length::Fixed(40.0))
                                    .content_fit(iced::ContentFit::Contain)
                                    .into()
                            };

                            item_row = item_row.push(icon_content);
                        }
                    }
                }

                // Name + subtitle (full path)
                let display_name = if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                };

                let name_color = if entry.is_dir { t.accent } else { t.text_main };

                let mut text_col = Column::new();
                text_col = text_col.push(text(display_name).size(20).color(name_color));
                text_col =
                    text_col.push(text(entry.full_path.clone()).size(14).color(t.text_muted));
                item_row = item_row.push(text_col.width(Length::Fill));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.text_main,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..t.accent_hover
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None,
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(item_button);
                special_item_idx += 1;
                let _ = idx; // used for iteration only
            }
        }

        // Add web search row if active
        if let Some(ref ws) = self.web_search_active {
            let is_selected = self.selected_index == special_item_idx;

            let mut ws_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

            // Try to resolve icon from icon_map
            if let Some(ref icon_name) = ws.icon {
                if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                    let icon_path = PathBuf::from(icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.to_lowercase() == "svg")
                            .unwrap_or(false);

                        let icon_content: Element<Message> = if is_svg {
                            svg(iced::widget::svg::Handle::from_path(&icon_path))
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        } else {
                            image(icon_path)
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        };

                        ws_row = ws_row.push(icon_content);
                    }
                }
            }

            if ws.query.is_empty() {
                // Hint row (not clickable)
                let hint = format!("Search {}: type your query...", ws.name);
                ws_row = ws_row.push(text(hint).size(20).color(t.text_muted));

                let ws_button =
                    button(ws_row)
                        .padding(12)
                        .width(Length::Fill)
                        .style(move |_theme, _status| {
                            let base_style = button::Style {
                                text_color: t.text_muted,
                                border: iced::Border {
                                    radius: 8.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            };

                            if is_selected {
                                button::Style {
                                    background: Some(iced::Background::Color(t.selection_bg)),
                                    border: iced::Border {
                                        color: t.accent,
                                        width: 1.0,
                                        radius: 8.0.into(),
                                    },
                                    ..base_style
                                }
                            } else {
                                button::Style {
                                    background: None,
                                    ..base_style
                                }
                            }
                        });

                items_column = items_column.push(ws_button);
            } else {
                // Clickable row with query
                let label = format!("Search {} for '{}'", ws.name, ws.query);
                ws_row = ws_row.push(text(label).size(20).color(t.accent));

                let ws_button = button(ws_row)
                    .on_press(Message::WebSearchSelected)
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.accent,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..t.accent_hover
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None,
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(ws_button);
            }
            special_item_idx += 1;
        }

        // Add currency help as first item if user typed just "$"
        if self.currency_help {
            let help_text = "Currency: $10 to EUR, $50 GBP to USD";
            let is_selected = self.selected_index == special_item_idx;

            let help_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(help_text).size(20).color(t.text_muted));

            let help_button =
                button(help_row)
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, _status| {
                        let base_style = button::Style {
                            text_color: t.text_muted,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            button::Style {
                                background: None,
                                ..base_style
                            }
                        }
                    });

            items_column = items_column.push(help_button);
            special_item_idx += 1;
        }

        // Add currency result/loading as first item if present
        if self.currency_loading {
            let loading_text = if let Some(ref req) = self.pending_currency_request {
                format!(
                    "Converting {} {} to {}...",
                    req.amount, req.from_currency, req.to_currency
                )
            } else {
                "Converting...".to_string()
            };

            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(loading_text).size(20).color(t.text_muted));

            let is_selected = self.selected_index == special_item_idx;

            let loading_button = button(loading_row).padding(12).width(Length::Fill).style(
                move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                },
            );

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if let Some(ref currency) = self.currency_result {
            let currency_text = format!(
                "{:.2} {} = {:.2} {} (rate: {:.4})",
                currency.request.amount,
                currency.request.from_currency,
                currency.converted_amount,
                currency.request.to_currency,
                currency.rate
            );

            let currency_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(currency_text).size(20).color(t.accent));

            let is_selected = self.selected_index == special_item_idx;

            let currency_button = button(currency_row)
                .on_press(Message::CurrencyResultCopied)
                .padding(12)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: t.accent,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        match status {
                            button::Status::Hovered => button::Style {
                                background: Some(iced::Background::Color(iced::Color {
                                    a: 0.1,
                                    ..t.accent_hover
                                })),
                                ..base_style
                            },
                            _ => button::Style {
                                background: None,
                                ..base_style
                            },
                        }
                    }
                });

            items_column = items_column.push(currency_button);
            special_item_idx += 1;
        }

        // Add multi-currency loading/results
        if self.multi_currency_loading {
            let loading_text = if let Some(ref req) = self.pending_multi_currency_request {
                format!(
                    "Converting {} {} to {}...",
                    req.amount,
                    req.from_currency,
                    req.to_currencies.join(", ")
                )
            } else {
                "Converting...".to_string()
            };

            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(loading_text).size(20).color(t.text_muted));

            let is_selected = self.selected_index == special_item_idx;

            let loading_button = button(loading_row).padding(12).width(Length::Fill).style(
                move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                },
            );

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if let Some(ref multi_result) = self.multi_currency_result {
            for (idx, conversion) in multi_result.conversions.iter().enumerate() {
                let conversion_text = format!(
                    "{:.2} {} = {:.2} {} (rate: {:.4})",
                    multi_result.amount,
                    multi_result.from_currency,
                    conversion.converted_amount,
                    conversion.to_currency,
                    conversion.rate
                );

                let conversion_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(conversion_text).size(20).color(t.accent));

                let is_selected = self.selected_index == special_item_idx;

                let conversion_button = button(conversion_row)
                    .on_press(Message::MultiCurrencyResultCopied(idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.accent,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..t.accent_hover
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None,
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(conversion_button);
                special_item_idx += 1;
            }
        }

        // Add calculator result if present
        if let Some(ref calc) = self.calculator_result {
            let result_str = if calc.result.fract() == 0.0 {
                format!("{}", calc.result as i64)
            } else {
                format!("{}", calc.result)
            };

            let calc_text = format!("= {} = {}", calc.expression, result_str);
            let calc_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(calc_text).size(20).color(t.accent));

            let is_selected = self.selected_index == special_item_idx;

            let calc_button = button(calc_row)
                .on_press(Message::CalculatorSelected)
                .padding(12)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: t.accent,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        match status {
                            button::Status::Hovered => button::Style {
                                background: Some(iced::Background::Color(iced::Color {
                                    a: 0.1,
                                    ..t.accent_hover
                                })),
                                ..base_style
                            },
                            _ => button::Style {
                                background: None,
                                ..base_style
                            },
                        }
                    }
                });

            items_column = items_column.push(calc_button);
            special_item_idx += 1;
        }

        if self.filtered_configs.is_empty()
            && !has_calculator
            && !has_currency
            && !has_script_filter
            && !has_file_browser
            && !has_web_search
        {
            let no_results = container(
                text("No matching results found.")
                    .size(18)
                    .color(t.text_muted),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center);

            // If no results, we just show the message in the scroll area
            items_column = items_column.push(no_results);
        } else {
            for (idx, &config_idx) in self.filtered_configs.iter().enumerate() {
                // Adjust display index for special items (currency, calculator)
                let display_idx = idx + special_item_idx;
                let config = &self.configs[config_idx];
                let description = config
                    .description
                    .clone()
                    .unwrap_or_else(|| config.binary.clone().unwrap_or_default());

                // Get icon path if available
                let mut icon_path = if !self.icon_map.is_empty() {
                    let icon_name = config
                        .icon
                        .as_ref()
                        .or(config.binary.as_ref())
                        .cloned()
                        .unwrap_or_default();
                    self.icon_map.get(&icon_name).cloned()
                } else {
                    None
                };

                if icon_path.is_none() {
                    let default_path = "assets/default_icon.svg";
                    if Path::new(default_path).exists() {
                        icon_path = Some(default_path.to_string());
                    }
                }

                // Build the row with optional icon
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                // Add icon if available
                if let Some(icon_path_str) = icon_path {
                    let icon_path = PathBuf::from(&icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.to_lowercase() == "svg")
                            .unwrap_or(false);

                        // Icon Container for consistent sizing and alignment
                        let icon_content: Element<Message> = if is_svg {
                            svg(iced::widget::svg::Handle::from_path(&icon_path))
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        } else {
                            image(icon_path)
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        };

                        item_row = item_row.push(icon_content);
                    }
                }

                // Text Content
                let text_widget = text(description).size(20).width(Length::Fill);
                item_row = item_row.push(text_widget);

                let is_selected = display_idx == self.selected_index;

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(display_idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.text_main,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..t.accent_hover
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None, // Transparent by default
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(item_button);
            }
        }

        let items_container = container(items_column)
            .id(self.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.scrollable_id.clone())
            .height(Length::Fill)
            .width(Length::Fill);

        // Hint bar above the search input
        let sep = span("  ·  ").size(12).color(t.border);
        let mut hint_spans: Vec<iced::widget::text::Span<'_, (), iced::Font>> = Vec::new();

        // Always-visible keybinding hints
        if self.max_history > 0 {
            hint_spans.push(span("Alt+P").size(12).color(t.accent));
            hint_spans.push(span("/").size(12).color(t.text_muted));
            hint_spans.push(span("N").size(12).color(t.accent));
            hint_spans.push(span(" history").size(12).color(t.text_muted));
        }
        if self.file_browser_active {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            hint_spans.push(span("Ctrl+H").size(12).color(t.accent));
            hint_spans.push(span(" toggle hidden").size(12).color(t.text_muted));
        }
        if !hint_spans.is_empty() {
            hint_spans.push(sep.clone());
        }
        hint_spans.push(span("Ctrl+/").size(12).color(t.accent));
        hint_spans.push(span(" help").size(12).color(t.text_muted));

        // Toggleable addon hints
        if self.show_hints {
            let mut addon_spans: Vec<iced::widget::text::Span<'_, (), iced::Font>> = Vec::new();
            if self.addons.calculator.enabled {
                addon_spans.push(span("math").size(12).color(t.text_muted));
            }
            if self.addons.currency.enabled {
                let trigger = self.addons.currency.trigger.as_deref().unwrap_or("$");
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(span(trigger.to_string()).size(12).color(t.accent));
                addon_spans.push(span(" currency").size(12).color(t.text_muted));
            }
            if self.addons.file_browser.enabled {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(span("/").size(12).color(t.accent));
                addon_spans.push(span(" files").size(12).color(t.text_muted));
            }
            for sf in &self.addons.script_filters {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(span(sf.keyword.clone()).size(12).color(t.accent));
                addon_spans.push(
                    span(format!(" {}", sf.name.to_lowercase()))
                        .size(12)
                        .color(t.text_muted),
                );
            }
            for ws in &self.addons.web_searches {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(span(ws.keyword.clone()).size(12).color(t.accent));
                addon_spans.push(
                    span(format!(" {}", ws.name.to_lowercase()))
                        .size(12)
                        .color(t.text_muted),
                );
            }
            if !addon_spans.is_empty() {
                hint_spans.push(span("\n").size(12));
                hint_spans.extend(addon_spans);
            }
        }

        let mut main_column = Column::new()
            .spacing(12)
            .width(Length::Fill)
            .height(Length::Fill);

        let hint_row = container(rich_text(hint_spans))
            .width(Length::Fill)
            .align_x(iced::Alignment::End);
        main_column = main_column.push(hint_row);

        // Main Layout
        let content = main_column
            .push(search_input)
            .push(container(items_scroll).padding(iced::Padding {
                top: 8.0,
                right: 4.0,
                bottom: 0.0,
                left: 0.0,
            }));

        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(t.bg_base)),
                border: iced::Border {
                    color: t.border,
                    width: 1.0,
                    radius: 16.0.into(),
                },
                text_color: Some(t.text_main),
                ..Default::default()
            })
            .into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        use iced::keyboard;
        use iced::keyboard::key::Named;
        use iced::{event, Event};

        event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowDown),
                ..
            }) => Some(Message::MoveDown),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowUp),
                ..
            }) => Some(Message::MoveUp),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Enter),
                ..
            }) => Some(Message::Submit),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Escape),
                ..
            }) => Some(Message::Cancel),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Tab),
                ..
            }) => Some(Message::FileBrowserTabComplete),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "p" && modifiers.alt() => Some(Message::HistoryPrevious),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "n" && modifiers.alt() => Some(Message::HistoryNext),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "h" && modifiers.control() => {
                Some(Message::FileBrowserToggleHidden)
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "/" && modifiers.control() => Some(Message::ToggleHints),
            Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            _ => None,
        })
    }

    fn filter_items(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_configs = (0..self.configs.len()).collect();
        } else {
            self.filtered_configs = fuzzy_match_configs(&self.configs, query);
        }
    }

    fn total_items(&self) -> usize {
        let mut offset = 0;
        if self.script_filter_loading {
            offset += 1;
        } else if let Some(ref sf_result) = self.script_filter_results {
            offset += sf_result.items.len();
        }
        if self.file_browser_active {
            offset += self.file_browser_entries.len();
        }
        if self.web_search_active.is_some() {
            offset += 1;
        }
        if self.currency_help {
            offset += 1;
        }
        if self.currency_result.is_some() || self.currency_loading {
            offset += 1;
        }
        if self.multi_currency_loading {
            offset += 1;
        } else if let Some(ref multi_result) = self.multi_currency_result {
            offset += multi_result.conversions.len();
        }
        if self.calculator_result.is_some() {
            offset += 1;
        }
        self.filtered_configs.len() + offset
    }

    /// Save the current search query to the command history.
    fn save_query_to_history(&mut self) {
        let query = self.search_query.trim().to_string();
        if !query.is_empty() && self.max_history > 0 {
            // Remove duplicate if present so the new entry goes to the end
            self.history.retain(|h| h != &query);
            self.history.push(query);
            // Trim to max_history
            if self.history.len() > self.max_history as usize {
                let excess = self.history.len() - self.max_history as usize;
                self.history.drain(..excess);
            }
            save_history(&self.history);
        }
    }
}

fn try_evaluate_math(query: &str) -> Option<CalculatorResult> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Check if expression looks like math: must contain an operator
    let has_operator = trimmed.contains('+')
        || trimmed.contains('-')
        || trimmed.contains('*')
        || trimmed.contains('/')
        || trimmed.contains('^')
        || trimmed.contains('%');

    // Check for function calls like sqrt(), sin(), etc.
    let has_function = trimmed.contains("sqrt")
        || trimmed.contains("sin")
        || trimmed.contains("cos")
        || trimmed.contains("tan")
        || trimmed.contains("log")
        || trimmed.contains("ln")
        || trimmed.contains("exp")
        || trimmed.contains("abs")
        || trimmed.contains("floor")
        || trimmed.contains("ceil");

    if !has_operator && !has_function {
        return None;
    }

    // Check valid start: digit, '(', '-', or '.'
    let first_char = trimmed.chars().next()?;
    let valid_start =
        first_char.is_ascii_digit() || first_char == '(' || first_char == '-' || first_char == '.';

    // Also allow function names at the start
    let starts_with_function = trimmed.starts_with("sqrt")
        || trimmed.starts_with("sin")
        || trimmed.starts_with("cos")
        || trimmed.starts_with("tan")
        || trimmed.starts_with("log")
        || trimmed.starts_with("ln")
        || trimmed.starts_with("exp")
        || trimmed.starts_with("abs")
        || trimmed.starts_with("floor")
        || trimmed.starts_with("ceil");

    if !valid_start && !starts_with_function {
        return None;
    }

    // Try to evaluate
    match meval::eval_str(trimmed) {
        Ok(result) if result.is_finite() => Some(CalculatorResult {
            expression: trimmed.to_string(),
            result,
        }),
        _ => None,
    }
}

fn fuzzy_match_configs(configs: &[RaffiConfig], query: &str) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();
    let mut matches: Vec<(usize, i64)> = configs
        .iter()
        .enumerate()
        .filter_map(|(idx, config)| {
            let description = config
                .description
                .as_deref()
                .or(config.binary.as_deref())
                .unwrap_or_default();
            matcher
                .fuzzy_match(description, query)
                .map(|score| (idx, score))
        })
        .collect();

    // Sort by score descending
    matches.sort_by(|a, b| b.1.cmp(&a.1));

    matches.into_iter().map(|(idx, _)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_match_configs() {
        let configs = vec![
            RaffiConfig {
                description: Some("Firefox".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                description: Some("Google Chrome".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                description: Some("Alacritty".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                binary: Some("code".to_string()),
                description: None,
                ..Default::default()
            },
        ];

        // Exact match
        let results = fuzzy_match_configs(&configs, "Firefox");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], 0);

        // Fuzzy match "fox" -> Firefox
        let results = fuzzy_match_configs(&configs, "fox");
        assert!(results.contains(&0));

        // Fuzzy match "chr" -> Google Chrome
        let results = fuzzy_match_configs(&configs, "chr");
        assert!(results.contains(&1));

        // Fuzzy match binary "od" -> code
        let results = fuzzy_match_configs(&configs, "od");
        assert!(results.contains(&3));

        // Ranking check: "o" matches Firefox, Google Chrome, code
        // "code" (idx 3) should likely be high for "o" if it starts with it or is short,
        // but let's just check we get results
        let results = fuzzy_match_configs(&configs, "o");
        assert!(results.len() >= 3);
        assert!(results.contains(&0)); // FirefOx
        assert!(results.contains(&1)); // GOOgle Chrome
        assert!(results.contains(&3)); // cOde
    }

    #[test]
    fn test_try_evaluate_math_basic_operations() {
        // Addition
        let result = try_evaluate_math("2+2");
        assert!(result.is_some());
        assert_eq!(result.as_ref().unwrap().result, 4.0);

        // Subtraction
        let result = try_evaluate_math("10-3");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 7.0);

        // Multiplication
        let result = try_evaluate_math("5*6");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 30.0);

        // Division
        let result = try_evaluate_math("20/4");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        // Power
        let result = try_evaluate_math("2^3");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 8.0);

        // Modulo
        let result = try_evaluate_math("17%5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 2.0);
    }

    #[test]
    fn test_try_evaluate_math_complex_expressions() {
        // Parentheses
        let result = try_evaluate_math("(10+5)*2");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 30.0);

        // Nested parentheses
        let result = try_evaluate_math("((2+3)*4)-5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 15.0);

        // Negative numbers
        let result = try_evaluate_math("-5+10");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        // Decimals
        let result = try_evaluate_math("3.5*2");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 7.0);
    }

    #[test]
    fn test_try_evaluate_math_functions() {
        // sqrt
        let result = try_evaluate_math("sqrt(16)");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 4.0);

        // abs
        let result = try_evaluate_math("abs(-5)");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        // sin(0) should be 0
        let result = try_evaluate_math("sin(0)");
        assert!(result.is_some());
        assert!((result.unwrap().result - 0.0).abs() < 0.0001);
    }

    #[test]
    fn test_try_evaluate_math_not_math() {
        // Regular text should not trigger calculator
        assert!(try_evaluate_math("firefox").is_none());
        assert!(try_evaluate_math("google chrome").is_none());
        assert!(try_evaluate_math("hello world").is_none());

        // Text with numbers but no operators
        assert!(try_evaluate_math("firefox123").is_none());

        // Empty string
        assert!(try_evaluate_math("").is_none());

        // Just whitespace
        assert!(try_evaluate_math("   ").is_none());
    }

    #[test]
    fn test_try_evaluate_math_invalid_expressions() {
        // Division by zero produces infinity, should be rejected
        let result = try_evaluate_math("1/0");
        assert!(result.is_none());

        // Invalid syntax - operator at start (no valid start character)
        assert!(try_evaluate_math("*5").is_none());

        // Text starting with letters should not match
        assert!(try_evaluate_math("x+5").is_none());
    }

    #[test]
    fn test_try_parse_currency_conversion_dollar_prefix() {
        // Basic pattern: "$10 to EUR" (defaults to USD)
        let result = try_parse_currency_conversion("$10 to EUR", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currency, "EUR");

        // With explicit source currency: "$10 GBP to USD"
        let result = try_parse_currency_conversion("$50 GBP to USD", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 50.0);
        assert_eq!(req.from_currency, "GBP");
        assert_eq!(req.to_currency, "USD");

        // Currency code attached: "$100EUR to JPY"
        let result = try_parse_currency_conversion("$100EUR to JPY", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 100.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "JPY");

        // With "in" instead of "to"
        let result = try_parse_currency_conversion("$25 in GBP", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 25.0);
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currency, "GBP");

        // Space after $
        let result = try_parse_currency_conversion("$ 10 to EUR", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currency, "EUR");

        // Case insensitive
        let result = try_parse_currency_conversion("$10 eur to gbp", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "GBP");

        // Decimal amount
        let result = try_parse_currency_conversion("$25.50 to JPY", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 25.50);
    }

    #[test]
    fn test_try_parse_currency_conversion_dollar_words() {
        // Pattern: "$10 to euros"
        let result = try_parse_currency_conversion("$10 to euros", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currency, "EUR");

        // Pattern: "$50 euros to dollars"
        let result = try_parse_currency_conversion("$50 euros to dollars", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 50.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");

        // Singular form
        let result = try_parse_currency_conversion("$1 to pound", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currency, "GBP");
    }

    #[test]
    fn test_try_parse_currency_conversion_invalid() {
        // No $ prefix - should not match
        assert!(try_parse_currency_conversion("10 USD to EUR", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("USD 10 to EUR", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("100 dollars to euros", "USD", "$").is_none());

        // Same currency
        assert!(try_parse_currency_conversion("$10 USD to USD", "USD", "$").is_none());

        // Invalid currency code
        assert!(try_parse_currency_conversion("$10 XYZ to EUR", "USD", "$").is_none());

        // Not a currency pattern
        assert!(try_parse_currency_conversion("hello world", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("10 + 5", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("", "USD", "$").is_none());

        // Missing parts
        assert!(try_parse_currency_conversion("$10 USD", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("$to EUR", "USD", "$").is_none());

        // Just $ or $ with space (help query, not conversion)
        assert!(try_parse_currency_conversion("$", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("$ ", "USD", "$").is_none());
    }

    #[test]
    fn test_is_currency_help_query() {
        assert!(is_currency_help_query("$", "$"));
        assert!(is_currency_help_query("$ ", "$"));
        assert!(is_currency_help_query(" $ ", "$"));

        assert!(!is_currency_help_query("$10", "$"));
        assert!(!is_currency_help_query("$10 to EUR", "$"));
        assert!(!is_currency_help_query("", "$"));
        assert!(!is_currency_help_query("hello", "$"));

        // Test with custom trigger
        assert!(is_currency_help_query("€", "€"));
        assert!(is_currency_help_query("€ ", "€"));
        assert!(!is_currency_help_query("€10", "€"));
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_default_currencies() {
        // Uses default currencies ["USD", "EUR", "GBP"] when config is empty
        let empty_config: Vec<String> = vec![];

        // "$10" → convert from USD (default) to EUR, GBP
        let result = try_parse_multi_currency_conversion("$10", &empty_config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "USD");
        assert!(req.to_currencies.contains(&"EUR".to_string()));
        assert!(req.to_currencies.contains(&"GBP".to_string()));
        assert!(!req.to_currencies.contains(&"USD".to_string()));

        // "$ 25.50" with space
        let result = try_parse_multi_currency_conversion("$ 25.50", &empty_config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 25.50);
        assert_eq!(req.from_currency, "USD");
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_with_source() {
        // Uses default currencies when config is empty
        let empty_config: Vec<String> = vec![];

        // "$10 EUR" → convert from EUR to USD, GBP
        let result = try_parse_multi_currency_conversion("$10 EUR", &empty_config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"GBP".to_string()));
        assert!(!req.to_currencies.contains(&"EUR".to_string()));

        // "$50 GBP" → convert from GBP to USD, EUR
        let result = try_parse_multi_currency_conversion("$50 GBP", &empty_config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 50.0);
        assert_eq!(req.from_currency, "GBP");
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"EUR".to_string()));
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_with_config() {
        // Custom config currencies
        let config = vec![
            "EUR".to_string(),
            "USD".to_string(),
            "JPY".to_string(),
            "CAD".to_string(),
        ];

        // "$10" → convert from EUR (default) to USD, JPY, CAD
        let result = try_parse_multi_currency_conversion("$10", &config, "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currencies.len(), 3);
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"JPY".to_string()));
        assert!(req.to_currencies.contains(&"CAD".to_string()));

        // "$10 JPY" → convert from JPY to EUR, USD, CAD
        let result = try_parse_multi_currency_conversion("$10 JPY", &config, "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "JPY");
        assert!(req.to_currencies.contains(&"EUR".to_string()));
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"CAD".to_string()));
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_explicit_syntax_returns_none() {
        let config: Vec<String> = vec![];

        // Explicit "to/in" syntax should return None (handled by existing parser)
        assert!(try_parse_multi_currency_conversion("$10 to EUR", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("$10 in GBP", &config, "USD", "$").is_none());
        assert!(
            try_parse_multi_currency_conversion("$10 USD to EUR", &config, "USD", "$").is_none()
        );
        assert!(
            try_parse_multi_currency_conversion("$10 euros to dollars", &config, "USD", "$")
                .is_none()
        );
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_invalid() {
        let config: Vec<String> = vec![];

        // Not starting with trigger
        assert!(try_parse_multi_currency_conversion("10 USD", &config, "USD", "$").is_none());

        // Invalid currency code
        assert!(try_parse_multi_currency_conversion("$10 XYZ", &config, "USD", "$").is_none());

        // Empty or whitespace
        assert!(try_parse_multi_currency_conversion("", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("   ", &config, "USD", "$").is_none());

        // Just $ (handled as help)
        assert!(try_parse_multi_currency_conversion("$", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("$ ", &config, "USD", "$").is_none());
    }

    #[test]
    fn test_try_parse_multi_currency_case_insensitive() {
        let config: Vec<String> = vec![];

        // Lowercase currency code
        let result = try_parse_multi_currency_conversion("$10 eur", &config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
    }

    #[test]
    fn test_try_parse_currency_conversion_with_custom_default() {
        // Test that custom default currency is used when no source is specified
        let result = try_parse_currency_conversion("$10 to USD", "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");

        // With word pattern
        let result = try_parse_currency_conversion("$10 to dollars", "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");
    }

    #[test]
    fn test_try_parse_multi_currency_with_custom_default() {
        // Test that custom default currency is used when no source is specified
        let config: Vec<String> = vec![];

        let result = try_parse_multi_currency_conversion("$10", &config, "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"GBP".to_string()));
        assert!(!req.to_currencies.contains(&"EUR".to_string()));
    }

    #[test]
    fn test_try_parse_currency_conversion_with_custom_trigger() {
        // Test with € trigger
        let result = try_parse_currency_conversion("€10 to USD", "EUR", "€");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");

        // Test with £ trigger
        let result = try_parse_currency_conversion("£50 to EUR", "GBP", "£");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 50.0);
        assert_eq!(req.from_currency, "GBP");
        assert_eq!(req.to_currency, "EUR");

        // Wrong trigger should not match
        assert!(try_parse_currency_conversion("$10 to EUR", "USD", "€").is_none());
    }

    #[test]
    fn test_try_parse_multi_currency_with_custom_trigger() {
        let config: Vec<String> = vec![];

        // Test with € trigger
        let result = try_parse_multi_currency_conversion("€10", &config, "EUR", "€");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");

        // Wrong trigger should not match
        assert!(try_parse_multi_currency_conversion("$10", &config, "USD", "€").is_none());
    }
}

fn load_mru_map() -> HashMap<String, u32> {
    if let Ok(path) = super::get_mru_cache_path() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut map = HashMap::new();
            for line in content.lines() {
                let mut parts = line.splitn(2, '|');
                if let (Some(desc), Some(count_str)) = (parts.next(), parts.next()) {
                    if let Ok(count) = count_str.parse::<u32>() {
                        map.insert(desc.to_string(), count);
                    }
                }
            }
            return map;
        }
    }
    HashMap::new()
}

fn save_mru_map(map: &HashMap<String, u32>) {
    if let Ok(path) = super::get_mru_cache_path() {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let content = entries
            .iter()
            .map(|(desc, count)| format!("{}|{}", desc, count))
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(e) = fs::write(&path, content) {
            eprintln!("Warning: Failed to save MRU cache to {:?}: {}", path, e);
        }
    }
}

/// Load command history from the history cache file.
/// Returns entries ordered oldest-first (newest at the end).
fn load_history(max_history: u32) -> Vec<String> {
    if max_history == 0 {
        return Vec::new();
    }
    if let Ok(path) = super::get_history_cache_path() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut entries: Vec<String> = content
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();
            // Keep only the most recent max_history entries
            if entries.len() > max_history as usize {
                let excess = entries.len() - max_history as usize;
                entries.drain(..excess);
            }
            return entries;
        }
    }
    Vec::new()
}

/// Save command history to the history cache file.
fn save_history(history: &[String]) {
    if let Ok(path) = super::get_history_cache_path() {
        let content = history.join("\n");
        if let Err(e) = fs::write(&path, content) {
            eprintln!("Warning: Failed to save history to {:?}: {}", path, e);
        }
    }
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(
    configs: &[RaffiConfig],
    addons: &AddonsConfig,
    no_icons: bool,
    initial_query: Option<&str>,
    theme_mode: &ThemeMode,
    theme_color_overrides: Option<&ThemeColorsConfig>,
    max_history: u32,
) -> Result<String> {
    let theme_colors = ThemeColors::from_mode_with_overrides(theme_mode, theme_color_overrides);
    let iced_theme = match theme_mode {
        ThemeMode::Dark => iced::Theme::Dark,
        ThemeMode::Light => iced::Theme::Light,
    };
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();

    // Clone configs and addons to own them for the 'static lifetime requirement
    let configs_owned = configs.to_vec();
    let addons_owned = addons.clone();

    let configs_for_new = configs_owned.clone();
    let addons_for_new = addons_owned.clone();
    let selected_item_for_new = selected_item_clone.clone();
    let initial_query_owned = initial_query.map(|s| s.to_string());

    let window_settings = window::Settings {
        size: iced::Size::new(800.0, 600.0),
        position: window::Position::Centered,
        decorations: false,
        transparent: true,
        visible: true,
        level: window::Level::AlwaysOnTop,
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "com.chmouel.raffi".to_string(),
            ..Default::default()
        },
        #[cfg(not(target_os = "linux"))]
        platform_specific: Default::default(),
        ..Default::default()
    };

    let result = iced::application(
        move || {
            LauncherApp::new(
                configs_for_new.clone(),
                addons_for_new.clone(),
                no_icons,
                selected_item_for_new.clone(),
                initial_query_owned.clone(),
                theme_colors,
                max_history,
            )
        },
        LauncherApp::update,
        LauncherApp::view,
    )
    .subscription(LauncherApp::subscription)
    .theme(move |_state: &LauncherApp| iced_theme.clone())
    .window(window_settings)
    .run();

    if let Err(e) = result {
        return Err(anyhow::anyhow!("Failed to run UI: {:?}", e));
    }

    // Retrieve the selected item from the shared state
    if let Ok(selected) = selected_item.lock() {
        if let Some(item) = selected.clone() {
            return Ok(item);
        }
    }

    Ok(String::new())
}
