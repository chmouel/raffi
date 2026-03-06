use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use iced::Task;
use regex::Regex;
use serde::Deserialize;

use super::state::{
    CurrencyConversion, CurrencyConversionRequest, CurrencyResult, Message, MultiCurrencyRequest,
    MultiCurrencyResult,
};

const DEFAULT_CURRENCIES: &[&str] = &["USD", "EUR", "GBP"];

#[derive(Debug, Deserialize)]
struct FrankfurterResponse {
    rates: HashMap<String, f64>,
}

const SUPPORTED_CURRENCIES: &[&str] = &[
    "EUR", "USD", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "HKD", "NZD", "SEK", "KRW", "SGD",
    "NOK", "MXN", "INR", "RUB", "ZAR", "TRY", "BRL", "TWD", "DKK", "PLN", "THB", "IDR", "HUF",
    "CZK", "ILS", "CLP", "PHP", "AED", "COP", "SAR", "MYR", "RON", "BGN", "ISK", "HRK",
];

static PATTERN_CURRENCY_CONVERSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(\d+(?:\.\d+)?)\s*([A-Z]{3})?\s*(?:to|in)\s+([A-Z]{3})$").unwrap()
});

static PATTERN_CURRENCY_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(\d+(?:\.\d+)?)\s*(dollars?|euros?|pounds?|yen|yuan)?\s*(?:to|in)\s+(dollars?|euros?|pounds?|yen|yuan)$").unwrap()
});

static PATTERN_SIMPLE_CURRENCY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(\d+(?:\.\d+)?)\s*([A-Z]{3})?$").unwrap());

pub(super) fn is_currency_help_query(query: &str, trigger: &str) -> bool {
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

pub(super) fn try_parse_currency_conversion(
    query: &str,
    default_currency: &str,
    trigger: &str,
) -> Option<CurrencyConversionRequest> {
    let trimmed = query.trim();
    if !trimmed.starts_with(trigger) {
        return None;
    }

    let after_trigger = &trimmed[trigger.len()..];

    if let Some(caps) = PATTERN_CURRENCY_CONVERSION.captures(after_trigger) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;
        let from = caps
            .get(2)
            .map(|capture| capture.as_str().to_uppercase())
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

    if let Some(caps) = PATTERN_CURRENCY_WORDS.captures(after_trigger) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;
        let from = caps
            .get(2)
            .and_then(|capture| word_to_currency(capture.as_str()))
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

pub(super) fn try_parse_multi_currency_conversion(
    query: &str,
    config_currencies: &[String],
    default_currency: &str,
    trigger: &str,
) -> Option<MultiCurrencyRequest> {
    let trimmed = query.trim();
    if !trimmed.starts_with(trigger) {
        return None;
    }

    let after_trigger = &trimmed[trigger.len()..];
    if PATTERN_CURRENCY_CONVERSION.is_match(after_trigger)
        || PATTERN_CURRENCY_WORDS.is_match(after_trigger)
    {
        return None;
    }

    if let Some(caps) = PATTERN_SIMPLE_CURRENCY.captures(after_trigger) {
        let amount: f64 = caps.get(1)?.as_str().parse().ok()?;
        let currencies: Vec<String> = if config_currencies.is_empty() {
            DEFAULT_CURRENCIES
                .iter()
                .map(|currency| currency.to_string())
                .collect()
        } else {
            config_currencies.to_vec()
        };

        if currencies.len() < 2 {
            return None;
        }

        let from_currency = if let Some(capture) = caps.get(2) {
            let code = capture.as_str().to_uppercase();
            if !is_valid_currency(&code) {
                return None;
            }
            code
        } else {
            default_currency.to_string()
        };

        let to_currencies: Vec<String> = currencies
            .iter()
            .filter(|currency| currency.to_uppercase() != from_currency.to_uppercase())
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

pub(super) fn fetch_exchange_rate(request: CurrencyConversionRequest) -> Task<Message> {
    let request_for_result = request.clone();
    Task::perform(
        async move { fetch_rate_blocking(&request) },
        move |result| Message::CurrencyConversionResult(request_for_result, result),
    )
}

fn fetch_rate_blocking(request: &CurrencyConversionRequest) -> Result<CurrencyResult, String> {
    crate::debug_log!(
        "currency: fetch rate {} {} -> {}",
        request.amount,
        request.from_currency,
        request.to_currency
    );
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
        .map_err(|error| format!("Network error: {}", error))?
        .body_mut()
        .read_json()
        .map_err(|error| format!("Parse error: {}", error))?;

    let rate = response
        .rates
        .get(&request.to_currency)
        .copied()
        .ok_or_else(|| "Rate not found".to_string())?;

    crate::debug_log!("currency: rate={rate} converted={}", request.amount * rate);
    Ok(CurrencyResult {
        request: request.clone(),
        converted_amount: request.amount * rate,
        rate,
    })
}

pub(super) fn fetch_multi_exchange_rates(request: MultiCurrencyRequest) -> Task<Message> {
    let request_for_result = request.clone();
    Task::perform(
        async move { fetch_multi_rates_blocking(&request) },
        move |result| Message::MultiCurrencyConversionResult(request_for_result, result),
    )
}

fn fetch_multi_rates_blocking(
    request: &MultiCurrencyRequest,
) -> Result<MultiCurrencyResult, String> {
    crate::debug_log!(
        "currency: fetch multi rates {} {} -> {:?}",
        request.amount,
        request.from_currency,
        request.to_currencies
    );
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
        .map_err(|error| format!("Network error: {}", error))?
        .body_mut()
        .read_json()
        .map_err(|error| format!("Parse error: {}", error))?;

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

#[cfg(test)]
mod tests {
    use super::{
        is_currency_help_query, try_parse_currency_conversion, try_parse_multi_currency_conversion,
    };

    #[test]
    fn test_is_currency_help_query() {
        assert!(is_currency_help_query("$", "$"));
        assert!(is_currency_help_query("$ ", "$"));
        assert!(!is_currency_help_query("$10", "$"));
    }

    #[test]
    fn test_try_parse_currency_conversion_dollar_prefix() {
        let result = try_parse_currency_conversion("$10 to EUR", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currency, "EUR");
    }

    #[test]
    fn test_try_parse_currency_conversion_dollar_words() {
        let result = try_parse_currency_conversion("$10 euros to dollars", "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");
    }

    #[test]
    fn test_try_parse_currency_conversion_invalid() {
        assert!(try_parse_currency_conversion("10 to EUR", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("$10 to XYZ", "USD", "$").is_none());
        assert!(try_parse_currency_conversion("$10 USD to USD", "USD", "$").is_none());
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_default_currencies() {
        let config: Vec<String> = vec![];
        let result = try_parse_multi_currency_conversion("$10", &config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "USD");
        assert_eq!(req.to_currencies.len(), 2);
        assert!(req.to_currencies.contains(&"EUR".to_string()));
        assert!(req.to_currencies.contains(&"GBP".to_string()));
    }

    #[test]
    fn test_try_parse_multi_currency_conversion_with_source() {
        let empty_config: Vec<String> = vec![];

        let result = try_parse_multi_currency_conversion("$10 EUR", &empty_config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"GBP".to_string()));
        assert!(!req.to_currencies.contains(&"EUR".to_string()));

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
        let config = vec![
            "EUR".to_string(),
            "USD".to_string(),
            "JPY".to_string(),
            "CAD".to_string(),
        ];

        let result = try_parse_multi_currency_conversion("$10", &config, "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currencies.len(), 3);
        assert!(req.to_currencies.contains(&"USD".to_string()));
        assert!(req.to_currencies.contains(&"JPY".to_string()));
        assert!(req.to_currencies.contains(&"CAD".to_string()));

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

        assert!(try_parse_multi_currency_conversion("10 USD", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("$10 XYZ", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("   ", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("$", &config, "USD", "$").is_none());
        assert!(try_parse_multi_currency_conversion("$ ", &config, "USD", "$").is_none());
    }

    #[test]
    fn test_try_parse_multi_currency_case_insensitive() {
        let config: Vec<String> = vec![];

        let result = try_parse_multi_currency_conversion("$10 eur", &config, "USD", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
    }

    #[test]
    fn test_try_parse_currency_conversion_with_custom_default() {
        let result = try_parse_currency_conversion("$10 to USD", "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");

        let result = try_parse_currency_conversion("$10 to dollars", "EUR", "$");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");
    }

    #[test]
    fn test_try_parse_multi_currency_with_custom_default() {
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
        let result = try_parse_currency_conversion("€10 to USD", "EUR", "€");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");
        assert_eq!(req.to_currency, "USD");

        let result = try_parse_currency_conversion("£50 to EUR", "GBP", "£");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 50.0);
        assert_eq!(req.from_currency, "GBP");
        assert_eq!(req.to_currency, "EUR");

        assert!(try_parse_currency_conversion("$10 to EUR", "USD", "€").is_none());
    }

    #[test]
    fn test_try_parse_multi_currency_with_custom_trigger() {
        let config: Vec<String> = vec![];

        let result = try_parse_multi_currency_conversion("€10", &config, "EUR", "€");
        assert!(result.is_some());
        let req = result.unwrap();
        assert_eq!(req.amount, 10.0);
        assert_eq!(req.from_currency, "EUR");

        assert!(try_parse_multi_currency_conversion("$10", &config, "USD", "€").is_none());
    }
}
