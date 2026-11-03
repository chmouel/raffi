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
use crate::{CurrencyAddonConfig, CurrencyRateProvider};

const DEFAULT_CURRENCIES: &[&str] = &["USD", "EUR", "GBP"];

#[derive(Debug, Deserialize)]
struct FrankfurterResponse {
    rates: HashMap<String, f64>,
}

/// The Bank of England's daily rates, served without a key by
/// allratestoday.com. Each row is `1 base = value quote`. Only contacted when
/// listed in `addons.currency.sources`.
const BANK_OF_ENGLAND_URL: &str = "https://allratestoday.com/api/open/central-bank/boe";

#[derive(Debug, Deserialize)]
struct BankTable {
    rates: Vec<BankQuote>,
}

#[derive(Debug, Deserialize)]
struct BankQuote {
    base: String,
    quote: String,
    #[serde(rename = "type")]
    kind: String,
    value: f64,
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
    query
        .trim()
        .strip_prefix(trigger)
        .is_some_and(|rest| rest.is_empty() || rest == " ")
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

/// How long the first source may take. Later sources get [`BACKUP_TIMEOUT`]
/// so a bad connection does not add a full wait per source.
const PRIMARY_TIMEOUT: Duration = Duration::from_secs(10);
const BACKUP_TIMEOUT: Duration = Duration::from_secs(4);

const FRANKFURTER_URL: &str = "https://api.frankfurter.dev/v1/latest";

/// One configured exchange rate source, ready to be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RateSource {
    provider: CurrencyRateProvider,
    url: String,
}

impl RateSource {
    /// The sources in the order the user wants them tried. Without a
    /// `sources` setting only Frankfurter is contacted.
    pub(super) fn from_config(config: &CurrencyAddonConfig) -> Vec<RateSource> {
        let configured = config.sources.as_deref().unwrap_or_default();
        if configured.is_empty() {
            return vec![RateSource::new(CurrencyRateProvider::Frankfurter, None)];
        }
        configured
            .iter()
            .map(|source| RateSource::new(source.provider(), source.url()))
            .collect()
    }

    fn new(provider: CurrencyRateProvider, url: Option<&str>) -> Self {
        let url = url
            .unwrap_or(match provider {
                CurrencyRateProvider::Frankfurter => FRANKFURTER_URL,
                CurrencyRateProvider::BankOfEngland => BANK_OF_ENGLAND_URL,
            })
            .to_string();
        Self { provider, url }
    }

    fn name(&self) -> &'static str {
        match self.provider {
            CurrencyRateProvider::Frankfurter => "Frankfurter",
            CurrencyRateProvider::BankOfEngland => "Bank of England",
        }
    }

    /// Rates for one unit of `from` in each of `to`, as far as this source
    /// has them. Nothing here decides whether the answer is good enough;
    /// see [`usable_rates`].
    fn fetch(
        &self,
        agent: &ureq::Agent,
        from: &str,
        to: &[String],
    ) -> Result<HashMap<String, f64>, String> {
        match self.provider {
            CurrencyRateProvider::Frankfurter => {
                let url = format!("{}?base={}&symbols={}", self.url, from, to.join(","));
                let response: FrankfurterResponse = read_json(agent, &url)?;
                Ok(response.rates)
            }
            CurrencyRateProvider::BankOfEngland => {
                let table: BankTable = read_json(agent, &self.url)?;
                Ok(bank_table_rates(&table, "GBP", from, to))
            }
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(agent: &ureq::Agent, url: &str) -> Result<T, String> {
    agent
        .get(url)
        .call()
        .map_err(|error| format!("network error: {error}"))?
        .body_mut()
        .read_json()
        .map_err(|error| format!("parse error: {error}"))
}

/// Rates for one unit of `from` in each of `to`, from the first source that
/// answers with at least one of them. The error names every source that was
/// tried and why it did not deliver, first source first.
fn fetch_rates(
    sources: &[RateSource],
    from: &str,
    to: &[String],
) -> Result<HashMap<String, f64>, String> {
    fetch_rates_with(sources, to, |source, timeout| {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build();
        let agent: ureq::Agent = config.into();
        source.fetch(&agent, from, to)
    })
}

fn fetch_rates_with<F>(
    sources: &[RateSource],
    to: &[String],
    mut fetch: F,
) -> Result<HashMap<String, f64>, String>
where
    F: FnMut(&RateSource, Duration) -> Result<HashMap<String, f64>, String>,
{
    if sources.is_empty() {
        return Err("No exchange rate source is configured".to_string());
    }
    let mut failures = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let timeout = if index == 0 {
            PRIMARY_TIMEOUT
        } else {
            BACKUP_TIMEOUT
        };
        match fetch(source, timeout).and_then(|rates| usable_rates(rates, to)) {
            Ok(rates) => {
                if !failures.is_empty() {
                    crate::debug_log!(
                        "currency: {} answered after {}",
                        source.name(),
                        failures.join("; ")
                    );
                }
                return Ok(rates);
            }
            Err(error) => {
                crate::debug_log!("currency: {} failed: {error}", source.name());
                failures.push(format!("{}: {error}", source.name()));
            }
        }
    }
    Err(format!(
        "Could not get an exchange rate ({})",
        failures.join("; ")
    ))
}

/// The requested rates that are actually usable. An answer with none of them
/// is a failure, so the next source gets a turn instead of an empty result
/// being shown as success.
fn usable_rates(
    rates: HashMap<String, f64>,
    to: &[String],
) -> Result<HashMap<String, f64>, String> {
    let rates: HashMap<String, f64> = rates
        .into_iter()
        .filter(|(currency, rate)| to.contains(currency) && rate.is_finite() && *rate > 0.0)
        .collect();
    if rates.is_empty() {
        return Err(format!("no rate for {}", to.join(", ")));
    }
    Ok(rates)
}

/// Rates for one unit of `from` in each of `to` that the bank's table can
/// provide. A currency the bank does not quote, as `from` or as a target,
/// yields no pairs rather than a guess.
fn bank_table_rates(
    table: &BankTable,
    home: &str,
    from: &str,
    to: &[String],
) -> HashMap<String, f64> {
    let units = per_home_units(table, home);
    to.iter()
        .filter_map(|to_currency| {
            cross_rate(&units, from, to_currency).map(|rate| (to_currency.clone(), rate))
        })
        .collect()
}

/// Units of each currency per unit of `home`, whichever way the bank quotes
/// the pair. Only reference rates count.
fn per_home_units(table: &BankTable, home: &str) -> HashMap<String, f64> {
    let mut units = HashMap::from([(home.to_string(), 1.0)]);
    for quote in &table.rates {
        if quote.kind != "reference" || quote.value <= 0.0 {
            continue;
        }
        if quote.base == home && quote.quote != home {
            units.insert(quote.quote.clone(), quote.value);
        } else if quote.quote == home && quote.base != home {
            units.insert(quote.base.clone(), 1.0 / quote.value);
        }
    }
    units
}

fn cross_rate(units: &HashMap<String, f64>, from: &str, to: &str) -> Option<f64> {
    Some(units.get(to)? / units.get(from)?)
}

pub(super) fn fetch_exchange_rate(
    request: CurrencyConversionRequest,
    sources: Vec<RateSource>,
) -> Task<Message> {
    let request_for_result = request.clone();
    Task::perform(
        async move { fetch_rate_blocking(&request, &sources) },
        move |result| Message::CurrencyConversionResult(request_for_result, result),
    )
}

fn fetch_rate_blocking(
    request: &CurrencyConversionRequest,
    sources: &[RateSource],
) -> Result<CurrencyResult, String> {
    crate::debug_log!(
        "currency: fetch rate {} {} -> {}",
        request.amount,
        request.from_currency,
        request.to_currency
    );
    let rates = fetch_rates(
        sources,
        &request.from_currency,
        std::slice::from_ref(&request.to_currency),
    )?;

    // `fetch_rates` only succeeds with at least one requested rate, and only
    // one was requested.
    let rate = rates.get(&request.to_currency).copied().ok_or_else(|| {
        format!(
            "Could not get an exchange rate ({} to {} missing from the answer)",
            request.from_currency, request.to_currency
        )
    })?;

    crate::debug_log!("currency: rate={rate} converted={}", request.amount * rate);
    Ok(CurrencyResult {
        request: request.clone(),
        converted_amount: request.amount * rate,
        rate,
    })
}

pub(super) fn fetch_multi_exchange_rates(
    request: MultiCurrencyRequest,
    sources: Vec<RateSource>,
) -> Task<Message> {
    let request_for_result = request.clone();
    Task::perform(
        async move { fetch_multi_rates_blocking(&request, &sources) },
        move |result| Message::MultiCurrencyConversionResult(request_for_result, result),
    )
}

fn fetch_multi_rates_blocking(
    request: &MultiCurrencyRequest,
    sources: &[RateSource],
) -> Result<MultiCurrencyResult, String> {
    crate::debug_log!(
        "currency: fetch multi rates {} {} -> {:?}",
        request.amount,
        request.from_currency,
        request.to_currencies
    );
    let rates = fetch_rates(sources, &request.from_currency, &request.to_currencies)?;

    let conversions: Vec<CurrencyConversion> = request
        .to_currencies
        .iter()
        .filter_map(|to_currency| {
            rates.get(to_currency).map(|&rate| CurrencyConversion {
                to_currency: to_currency.clone(),
                converted_amount: request.amount * rate,
                rate,
            })
        })
        .collect();

    if conversions.is_empty() {
        return Err(format!(
            "Could not get an exchange rate ({} to {} missing from the answer)",
            request.from_currency,
            request.to_currencies.join(", ")
        ));
    }

    Ok(MultiCurrencyResult {
        amount: request.amount,
        from_currency: request.from_currency.clone(),
        conversions,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{
        bank_table_rates, cross_rate, fetch_rates_with, is_currency_help_query, per_home_units,
        try_parse_currency_conversion, try_parse_multi_currency_conversion, usable_rates,
        BankQuote, BankTable, RateSource, BACKUP_TIMEOUT, BANK_OF_ENGLAND_URL, FRANKFURTER_URL,
        PRIMARY_TIMEOUT,
    };
    use crate::{CurrencyAddonConfig, CurrencyRateProvider, CurrencyRateSource};

    fn quote(base: &str, quote: &str, kind: &str, value: f64) -> BankQuote {
        BankQuote {
            base: base.to_string(),
            quote: quote.to_string(),
            kind: kind.to_string(),
            value,
        }
    }

    fn bank_table() -> BankTable {
        BankTable {
            rates: vec![
                quote("GBP", "USD", "reference", 1.25),
                quote("EUR", "GBP", "reference", 0.8),
                quote("GBP", "JPY", "buy", 190.0),
                quote("GBP", "CHF", "reference", 0.0),
            ],
        }
    }

    fn targets(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|code| code.to_string()).collect()
    }

    fn rates(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs
            .iter()
            .map(|(code, rate)| (code.to_string(), *rate))
            .collect()
    }

    #[test]
    fn test_bank_table_cross_rates() {
        let units = per_home_units(&bank_table(), "GBP");
        assert_eq!(units.len(), 3);
        assert!((cross_rate(&units, "EUR", "USD").unwrap() - 1.0).abs() < 1e-9);
        assert!((cross_rate(&units, "USD", "GBP").unwrap() - 0.8).abs() < 1e-9);
        assert!(cross_rate(&units, "GBP", "JPY").is_none());
    }

    #[test]
    fn test_bank_table_rates_drops_pairs_the_bank_does_not_quote() {
        let table = bank_table();
        // Target not in the table: that pair is dropped, the others stay.
        let found = bank_table_rates(&table, "GBP", "EUR", &targets(&["USD", "JPY", "GBP"]));
        assert_eq!(found.len(), 2);
        assert!(found.contains_key("USD"));
        assert!(found.contains_key("GBP"));
        assert!(!found.contains_key("JPY"));
        // Source not in the table: no pair can be computed, none is kept.
        let found = bank_table_rates(&table, "GBP", "INR", &targets(&["USD", "EUR", "GBP"]));
        assert!(found.is_empty());
    }

    #[test]
    fn test_bank_table_without_reference_rows_is_not_a_success() {
        let table = BankTable {
            rates: vec![
                quote("GBP", "USD", "buy", 1.25),
                quote("GBP", "EUR", "sell", 1.17),
            ],
        };
        let found = bank_table_rates(&table, "GBP", "USD", &targets(&["EUR", "GBP"]));
        assert!(found.is_empty());
        let outcome = usable_rates(found, &targets(&["EUR", "GBP"]));
        assert_eq!(outcome, Err("no rate for EUR, GBP".to_string()));
    }

    #[test]
    fn test_usable_rates_keeps_only_requested_positive_rates() {
        let answer = rates(&[("EUR", 0.9), ("JPY", 0.0), ("CHF", f64::NAN), ("SEK", 10.0)]);
        let usable = usable_rates(answer, &targets(&["EUR", "JPY", "CHF"])).unwrap();
        assert_eq!(usable, rates(&[("EUR", 0.9)]));
        assert!(usable_rates(HashMap::new(), &targets(&["EUR"])).is_err());
    }

    #[test]
    fn test_sources_default_to_frankfurter_only() {
        let config = CurrencyAddonConfig::default();
        let sources = RateSource::from_config(&config);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].provider, CurrencyRateProvider::Frankfurter);
        assert_eq!(sources[0].url, FRANKFURTER_URL);

        let config = CurrencyAddonConfig {
            sources: Some(Vec::new()),
            ..CurrencyAddonConfig::default()
        };
        assert_eq!(RateSource::from_config(&config), sources);
    }

    #[test]
    fn test_sources_follow_config_order_and_url() {
        let config = CurrencyAddonConfig {
            sources: Some(vec![
                CurrencyRateSource::Provider(CurrencyRateProvider::BankOfEngland),
                CurrencyRateSource::Custom {
                    provider: CurrencyRateProvider::Frankfurter,
                    url: "https://rates.example.org/latest".to_string(),
                },
            ]),
            ..CurrencyAddonConfig::default()
        };
        let sources = RateSource::from_config(&config);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].provider, CurrencyRateProvider::BankOfEngland);
        assert_eq!(sources[0].url, BANK_OF_ENGLAND_URL);
        assert_eq!(sources[1].provider, CurrencyRateProvider::Frankfurter);
        assert_eq!(sources[1].url, "https://rates.example.org/latest");
    }

    fn two_sources() -> Vec<RateSource> {
        vec![
            RateSource::new(CurrencyRateProvider::Frankfurter, None),
            RateSource::new(CurrencyRateProvider::BankOfEngland, None),
        ]
    }

    #[test]
    fn test_fetch_rates_uses_first_source_when_it_answers() {
        let mut calls = Vec::new();
        let result = fetch_rates_with(&two_sources(), &targets(&["EUR"]), |source, timeout| {
            calls.push((source.provider, timeout));
            Ok(rates(&[("EUR", 0.9)]))
        });
        assert_eq!(result, Ok(rates(&[("EUR", 0.9)])));
        assert_eq!(
            calls,
            vec![(CurrencyRateProvider::Frankfurter, PRIMARY_TIMEOUT)]
        );
    }

    #[test]
    fn test_fetch_rates_backup_gets_shorter_timeout() {
        let mut timeouts = Vec::new();
        let result = fetch_rates_with(&two_sources(), &targets(&["EUR"]), |source, timeout| {
            timeouts.push(timeout);
            match source.provider {
                CurrencyRateProvider::Frankfurter => Err("network error: timed out".to_string()),
                CurrencyRateProvider::BankOfEngland => Ok(rates(&[("EUR", 0.9)])),
            }
        });
        assert_eq!(result, Ok(rates(&[("EUR", 0.9)])));
        assert_eq!(timeouts, vec![PRIMARY_TIMEOUT, BACKUP_TIMEOUT]);
        assert!(BACKUP_TIMEOUT < PRIMARY_TIMEOUT);
        assert!(PRIMARY_TIMEOUT + BACKUP_TIMEOUT <= Duration::from_secs(15));
    }

    #[test]
    fn test_fetch_rates_empty_answer_moves_on_to_next_source() {
        let result = fetch_rates_with(&two_sources(), &targets(&["EUR"]), |source, _| match source
            .provider
        {
            CurrencyRateProvider::Frankfurter => Ok(HashMap::new()),
            CurrencyRateProvider::BankOfEngland => Ok(rates(&[("EUR", 0.9)])),
        });
        assert_eq!(result, Ok(rates(&[("EUR", 0.9)])));
    }

    #[test]
    fn test_fetch_rates_reports_every_failure_first_source_first() {
        let result =
            fetch_rates_with(
                &two_sources(),
                &targets(&["EUR", "INR"]),
                |source, _| match source.provider {
                    CurrencyRateProvider::Frankfurter => {
                        Err("network error: connection refused".to_string())
                    }
                    CurrencyRateProvider::BankOfEngland => Ok(HashMap::new()),
                },
            );
        assert_eq!(
            result,
            Err(
                "Could not get an exchange rate (Frankfurter: network error: connection refused; \
                 Bank of England: no rate for EUR, INR)"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_fetch_rates_without_sources_is_an_error() {
        let result = fetch_rates_with(&[], &targets(&["EUR"]), |_, _| Ok(rates(&[("EUR", 0.9)])));
        assert_eq!(
            result,
            Err("No exchange rate source is configured".to_string())
        );
    }

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
