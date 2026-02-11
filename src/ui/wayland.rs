use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, snap_to};
use iced::widget::{
    button, column, container, image, scrollable, svg, text, text_input, Column, Id, Row,
};
use iced::window;
use iced::{Element, Length, Task};
use lazy_static::lazy_static;
use regex::Regex;
use serde::Deserialize;

type ContainerId = Id;
type ScrollableId = Id;
type TextInputId = Id;

use super::UI;
use crate::{read_icon_map, AddonsConfig, RaffiConfig};

// --- Theme Colors (Dracula-ish with transparency) ---
const COLOR_BG_BASE: iced::Color = iced::Color {
    r: 0.15,
    g: 0.16,
    b: 0.21,
    a: 0.95,
}; // Dark Blue-Grey
const COLOR_BG_INPUT: iced::Color = iced::Color {
    r: 0.26,
    g: 0.27,
    b: 0.35,
    a: 1.0,
}; // Lighter Blue-Grey
const COLOR_ACCENT: iced::Color = iced::Color {
    r: 0.74,
    g: 0.57,
    b: 0.97,
    a: 1.0,
}; // Purple
const COLOR_ACCENT_HOVER: iced::Color = iced::Color {
    r: 0.54,
    g: 0.91,
    b: 0.99,
    a: 1.0,
}; // Cyan
const COLOR_TEXT_MAIN: iced::Color = iced::Color::WHITE;
const COLOR_TEXT_MUTED: iced::Color = iced::Color {
    r: 0.38,
    g: 0.44,
    b: 0.64,
    a: 1.0,
}; // Blueish Grey
const COLOR_SELECTION_BG: iced::Color = iced::Color {
    r: 0.27,
    g: 0.29,
    b: 0.36,
    a: 0.8,
}; // Selection HL
const COLOR_BORDER: iced::Color = iced::Color {
    r: 0.38,
    g: 0.44,
    b: 0.64,
    a: 0.5,
};

/// Wayland UI implementation using iced
pub struct WaylandUI;

impl UI for WaylandUI {
    fn show(
        &self,
        configs: &[RaffiConfig],
        addons: &AddonsConfig,
        no_icons: bool,
    ) -> Result<String> {
        run_wayland_ui(configs, addons, no_icons)
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

lazy_static! {
    // Pattern: "10 to EUR", "10 EUR to GBP", "10EUR to GBP" (trigger prefix stripped before matching)
    // Captures: amount, optional source currency, target currency
    static ref PATTERN_CURRENCY_CONVERSION: Regex = Regex::new(
        r"(?i)^\s*(\d+(?:\.\d+)?)\s*([A-Z]{3})?\s*(?:to|in)\s+([A-Z]{3})$"
    ).unwrap();

    // Pattern with word currencies: "10 euros to dollars" (trigger prefix stripped before matching)
    static ref PATTERN_CURRENCY_WORDS: Regex = Regex::new(
        r"(?i)^\s*(\d+(?:\.\d+)?)\s*(dollars?|euros?|pounds?|yen|yuan)?\s*(?:to|in)\s+(dollars?|euros?|pounds?|yen|yuan)$"
    ).unwrap();

    // Pattern: "10" or "10 EUR" (simple syntax without "to/in", trigger prefix stripped before matching)
    static ref PATTERN_SIMPLE_CURRENCY: Regex = Regex::new(
        r"(?i)^\s*(\d+(?:\.\d+)?)\s*([A-Z]{3})?$"
    ).unwrap();
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
}

impl LauncherApp {
    fn new(
        mut configs: Vec<RaffiConfig>,
        addons: AddonsConfig,
        no_icons: bool,
        selected_item: SharedSelection,
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

        let filtered_configs: Vec<usize> = (0..configs.len()).collect();
        let search_input_id = TextInputId::unique();
        let scrollable_id = ScrollableId::unique();
        let items_container_id = ContainerId::unique();

        (
            LauncherApp {
                configs,
                filtered_configs,
                search_query: String::new(),
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
            },
            focus(search_input_id),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => {
                self.search_query = query.clone();
                self.filter_items(&query);
                self.calculator_result = if self.addons.calculator.enabled {
                    try_evaluate_math(&query)
                } else {
                    None
                };
                self.selected_index = 0;
                // Regenerate IDs to force complete view refresh
                self.scrollable_id = ScrollableId::unique();
                self.items_container_id = ContainerId::unique();
                self.view_generation = self.view_generation.wrapping_add(1);

                let mut tasks: Vec<Task<Message>> = Vec::new();

                // Check for script filter keyword match
                let mut script_filter_matched = false;
                let trimmed = query.trim();
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
                        if let Some(ref action_tpl) = self.script_filter_action {
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
                iced::exit()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        // --- Search Input Styling ---
        let search_input = text_input("Type to search...", &self.search_query)
            .id(self.search_input_id.clone())
            .on_input(Message::SearchChanged)
            .on_submit(Message::Submit)
            .padding(16)
            .size(24)
            .style(|_theme, status| {
                let border_color = if matches!(status, text_input::Status::Focused { .. }) {
                    COLOR_ACCENT
                } else {
                    COLOR_BORDER
                };

                text_input::Style {
                    background: iced::Background::Color(COLOR_BG_INPUT),
                    border: iced::Border {
                        radius: 12.0.into(),
                        width: 1.0,
                        color: border_color,
                    },
                    placeholder: COLOR_TEXT_MUTED,
                    value: COLOR_TEXT_MAIN,
                    selection: COLOR_ACCENT,
                    icon: COLOR_TEXT_MUTED,
                }
            })
            .width(Length::Fill);

        // --- List Items ---
        let mut items_column = Column::new().spacing(6);

        // Track special items for index offset calculation
        let has_script_filter = self.script_filter_results.is_some() || self.script_filter_loading;
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
                .push(text(loading_text).size(20).color(COLOR_TEXT_MUTED));

            let is_selected = self.selected_index == special_item_idx;

            let loading_button = button(loading_row).padding(12).width(Length::Fill).style(
                move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: COLOR_TEXT_MUTED,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                            border: iced::Border {
                                color: COLOR_ACCENT,
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
                        let expanded = crate::expand_tilde(&p);
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
                text_col = text_col.push(text(item.title.clone()).size(20).color(COLOR_TEXT_MAIN));
                if let Some(ref subtitle) = item.subtitle {
                    text_col =
                        text_col.push(text(subtitle.clone()).size(14).color(COLOR_TEXT_MUTED));
                }
                item_row = item_row.push(text_col.width(Length::Fill));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: COLOR_TEXT_MAIN,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                                border: iced::Border {
                                    color: COLOR_ACCENT,
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
                                        ..COLOR_ACCENT_HOVER
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

        // Add currency help as first item if user typed just "$"
        if self.currency_help {
            let help_text = "Currency: $10 to EUR, $50 GBP to USD";
            let is_selected = self.selected_index == special_item_idx;

            let help_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(help_text).size(20).color(COLOR_TEXT_MUTED));

            let help_button =
                button(help_row)
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, _status| {
                        let base_style = button::Style {
                            text_color: COLOR_TEXT_MUTED,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                                border: iced::Border {
                                    color: COLOR_ACCENT,
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
                .push(text(loading_text).size(20).color(COLOR_TEXT_MUTED));

            let is_selected = self.selected_index == special_item_idx;

            let loading_button = button(loading_row).padding(12).width(Length::Fill).style(
                move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: COLOR_TEXT_MUTED,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                            border: iced::Border {
                                color: COLOR_ACCENT,
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
                .push(text(currency_text).size(20).color(COLOR_ACCENT));

            let is_selected = self.selected_index == special_item_idx;

            let currency_button = button(currency_row)
                .on_press(Message::CurrencyResultCopied)
                .padding(12)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: COLOR_ACCENT,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                            border: iced::Border {
                                color: COLOR_ACCENT,
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
                                    ..COLOR_ACCENT_HOVER
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
                .push(text(loading_text).size(20).color(COLOR_TEXT_MUTED));

            let is_selected = self.selected_index == special_item_idx;

            let loading_button = button(loading_row).padding(12).width(Length::Fill).style(
                move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: COLOR_TEXT_MUTED,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                            border: iced::Border {
                                color: COLOR_ACCENT,
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
                    .push(text(conversion_text).size(20).color(COLOR_ACCENT));

                let is_selected = self.selected_index == special_item_idx;

                let conversion_button = button(conversion_row)
                    .on_press(Message::MultiCurrencyResultCopied(idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: COLOR_ACCENT,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                                border: iced::Border {
                                    color: COLOR_ACCENT,
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
                                        ..COLOR_ACCENT_HOVER
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
                .push(text(calc_text).size(20).color(COLOR_ACCENT));

            let is_selected = self.selected_index == special_item_idx;

            let calc_button = button(calc_row)
                .on_press(Message::CalculatorSelected)
                .padding(12)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: COLOR_ACCENT,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                            border: iced::Border {
                                color: COLOR_ACCENT,
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
                                    ..COLOR_ACCENT_HOVER
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
        {
            let no_results = container(
                text("No matching results found.")
                    .size(18)
                    .color(COLOR_TEXT_MUTED),
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
                            text_color: COLOR_TEXT_MAIN,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                                border: iced::Border {
                                    color: COLOR_ACCENT,
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
                                        ..COLOR_ACCENT_HOVER
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

        // Main Layout
        let content = column![
            search_input,
            container(items_scroll).padding(iced::Padding {
                top: 8.0,
                right: 4.0,
                bottom: 0.0,
                left: 0.0
            })
        ]
        .spacing(12)
        .width(Length::Fill)
        .height(Length::Fill);

        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(COLOR_BG_BASE)),
                border: iced::Border {
                    color: COLOR_BORDER,
                    width: 1.0,
                    radius: 16.0.into(),
                },
                text_color: Some(COLOR_TEXT_MAIN),
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

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(
    configs: &[RaffiConfig],
    addons: &AddonsConfig,
    no_icons: bool,
) -> Result<String> {
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();

    // Clone configs and addons to own them for the 'static lifetime requirement
    let configs_owned = configs.to_vec();
    let addons_owned = addons.clone();

    let configs_for_new = configs_owned.clone();
    let addons_for_new = addons_owned.clone();
    let selected_item_for_new = selected_item_clone.clone();

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
            )
        },
        LauncherApp::update,
        LauncherApp::view,
    )
    .subscription(LauncherApp::subscription)
    .theme(|_state: &LauncherApp| iced::Theme::Dark)
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
