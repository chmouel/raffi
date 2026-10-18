use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use evalexpr::{
    eval_with_context, ContextWithMutableFunctions, ContextWithMutableVariables, Function,
    HashMapContext, Value,
};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use super::state::CalculatorResult;
use super::types::{InlineProvider, InlineResult, InlineRow};
use crate::ui::get_history_cache_path;
use crate::{RaffiConfig, SortMode};

#[derive(Debug, Clone)]
pub(super) struct MruEntry {
    pub count: u32,
    pub last_used: u64,
}

pub(super) fn try_evaluate_math(query: &str) -> Option<CalculatorResult> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }

    let has_operator = trimmed.contains('+')
        || trimmed.contains('-')
        || trimmed.contains('*')
        || trimmed.contains('/')
        || trimmed.contains('^')
        || trimmed.contains('%');

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

    let first_char = trimmed.chars().next()?;
    let valid_start =
        first_char.is_ascii_digit() || first_char == '(' || first_char == '-' || first_char == '.';

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

    match eval_math(trimmed) {
        Some(result) if result.is_finite() => Some(CalculatorResult {
            expression: trimmed.to_string(),
            result,
        }),
        _ => None,
    }
}

/// Evaluate a mathematical expression with the semantics the calculator has
/// always had: `^` is exponentiation, arithmetic happens in f64, and the
/// functions `sqrt`, `sin`, `cos`, `tan`, `log` (base 10), `ln`, `exp`,
/// `abs`, `floor`, `ceil` plus the `pi` and `e` constants are available.
fn eval_math(expression: &str) -> Option<f64> {
    static MATH_CONTEXT: LazyLock<HashMapContext> = LazyLock::new(|| {
        let mut context = HashMapContext::new();
        for (name, function) in [
            ("sqrt", f64::sqrt as fn(f64) -> f64),
            ("sin", f64::sin),
            ("cos", f64::cos),
            ("tan", f64::tan),
            ("log", f64::log10),
            ("ln", f64::ln),
            ("exp", f64::exp),
            ("abs", f64::abs),
            ("floor", f64::floor),
            ("ceil", f64::ceil),
        ] {
            context
                .set_function(
                    name.into(),
                    Function::new(move |argument: &Value| {
                        Ok(Value::Float(function(argument.as_number()?)))
                    }),
                )
                .expect("context accepts function definitions");
        }
        context
            .set_value("pi".into(), Value::Float(std::f64::consts::PI))
            .expect("context accepts value definitions");
        context
            .set_value("e".into(), Value::Float(std::f64::consts::E))
            .expect("context accepts value definitions");
        context
    });

    let floated = force_float_literals(expression);
    eval_with_context(&floated, &*MATH_CONTEXT)
        .ok()?
        .as_number()
        .ok()
}

/// Rewrite an expression so arithmetic stays in f64: evalexpr evaluates `/`
/// and `%` with integer semantics when both operands are integers, so plain
/// integer literals gain a `.0` suffix. Scientific-notation literals are
/// left untouched.
fn force_float_literals(expression: &str) -> String {
    let chars: Vec<char> = expression.chars().collect();
    let mut out = String::with_capacity(expression.len() + 4);
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            // Exponent part of a scientific-notation literal, as in `1e-3`.
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let run: String = chars[start..i].iter().collect();
            let is_float = run.contains('.') || run.contains('e') || run.contains('E');
            out.push_str(&run);
            if !is_float {
                out.push_str(".0");
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub(super) fn format_calculator_value(result: f64) -> String {
    if result.fract() == 0.0 {
        format!("{}", result as i64)
    } else {
        format!("{result}")
    }
}

pub(super) fn calculator_inline_result(calc: &CalculatorResult) -> InlineResult {
    let value = format_calculator_value(calc.result);
    InlineResult {
        provider: InlineProvider::Calculator,
        rows: vec![InlineRow {
            title: format!("= {} = {value}", calc.expression),
            subtitle: None,
            copy_value: value,
            alt_copy_value: None,
            swatch: None,
            preview: Some(super::preview::calculator_preview(calc)),
        }],
    }
}

/// Whether every word of `query` appears in `text`, ignoring case, so a
/// fuzzy match scattered over unrelated words is turned down.
pub(super) fn contains_words(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}

/// Whether `title` starts with `query`, or has a word that does, ignoring
/// case.
pub(super) fn word_prefix_match(title: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return false;
    }
    let title = title.to_lowercase();
    let mut before = None;
    title.char_indices().any(|(at, current)| {
        let starts = before.is_none_or(|before: char| !before.is_alphanumeric())
            && title[at..].starts_with(&query);
        before = Some(current);
        starts
    })
}

/// `log_query` is what the debug log shows in place of `query`, so
/// sensitive queries can be redacted.
pub(super) fn fuzzy_match_configs(
    configs: &[RaffiConfig],
    query: &str,
    log_query: &str,
) -> Vec<usize> {
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

    matches.sort_by_key(|b| std::cmp::Reverse(b.1));
    let result: Vec<usize> = matches.into_iter().map(|(idx, _)| idx).collect();
    crate::debug_log!("fuzzy_match: query={log_query:?} results={}", result.len());
    result
}

/// Parse MRU cache lines. Fields are split from the right so keys may
/// contain `|`.
pub(super) fn parse_mru_map(content: &str) -> HashMap<String, MruEntry> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.rsplitn(3, '|').collect();
        match parts.len() {
            3 => {
                // New format: desc|count|timestamp
                if let (Ok(count), Ok(ts)) = (parts[1].parse::<u32>(), parts[0].parse::<u64>()) {
                    map.insert(
                        parts[2].to_string(),
                        MruEntry {
                            count,
                            last_used: ts,
                        },
                    );
                }
            }
            2 => {
                // Old format: desc|count (auto-migrate, ts=0)
                if let Ok(count) = parts[0].parse::<u32>() {
                    map.insert(
                        parts[1].to_string(),
                        MruEntry {
                            count,
                            last_used: 0,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    map
}

pub(super) fn load_mru_map(path: Option<&Path>) -> HashMap<String, MruEntry> {
    let Some(content) = path.and_then(|path| fs::read_to_string(path).ok()) else {
        return HashMap::new();
    };
    let map = parse_mru_map(&content);
    crate::debug_log!("mru: loaded {} entries from {path:?}", map.len());
    map
}

/// Count one more use of `key`, now.
pub(super) fn record_mru_use(map: &mut HashMap<String, MruEntry>, key: String) {
    let entry = map.entry(key).or_insert(MruEntry {
        count: 0,
        last_used: 0,
    });
    entry.count += 1;
    entry.last_used = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
}

/// Highest count, oldest non-zero timestamp and newest timestamp, as needed
/// by `mru_sort_key`.
pub(super) fn mru_bounds(map: &HashMap<String, MruEntry>) -> (u32, u64, u64) {
    let max_count = map.values().map(|e| e.count).max().unwrap_or(0);
    let min_ts = map
        .values()
        .map(|e| e.last_used)
        .filter(|&t| t > 0)
        .min()
        .unwrap_or(0);
    let max_ts = map.values().map(|e| e.last_used).max().unwrap_or(0);
    (max_count, min_ts, max_ts)
}

pub(super) fn save_mru_map(path: Option<&Path>, map: &HashMap<String, MruEntry>) {
    crate::debug_log!("mru: saving {} entries to {path:?}", map.len());
    if let Some(path) = path {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.1.count));
        let content = entries
            .iter()
            .map(|(desc, entry)| format!("{}|{}|{}", desc, entry.count, entry.last_used))
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(error) = fs::write(path, content) {
            eprintln!("Warning: Failed to save MRU cache to {:?}: {}", path, error);
        }
    }
}

/// Compute a sort key for a config entry based on the active sort mode.
/// Returns a value where higher = should appear first (use `Reverse` when sorting).
pub(super) fn mru_sort_key(
    description: &str,
    mru_map: &HashMap<String, MruEntry>,
    sort_mode: &SortMode,
    max_count: u32,
    min_ts: u64,
    max_ts: u64,
) -> u64 {
    let entry = match mru_map.get(description) {
        Some(e) => e,
        None => return 0,
    };
    match sort_mode {
        SortMode::Frequency => u64::from(entry.count),
        SortMode::Recency => entry.last_used,
        SortMode::Hybrid => {
            let freq_norm = if max_count > 0 {
                f64::from(entry.count) / f64::from(max_count)
            } else {
                0.0
            };
            let recency_norm = if max_ts > min_ts {
                (entry.last_used.saturating_sub(min_ts)) as f64 / (max_ts - min_ts) as f64
            } else {
                0.0
            };
            let score = 0.4 * freq_norm + 0.6 * recency_norm;
            // Scale to u64 for integer-based sorting (multiply by 1_000_000 for precision)
            (score * 1_000_000.0) as u64
        }
    }
}

pub(super) fn load_history(max_history: u32) -> Vec<String> {
    if max_history == 0 {
        return Vec::new();
    }
    if let Ok(path) = get_history_cache_path() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut entries: Vec<String> = content
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| line.to_string())
                .collect();
            if entries.len() > max_history as usize {
                let excess = entries.len() - max_history as usize;
                entries.drain(..excess);
            }
            crate::debug_log!("history: loaded {} entries", entries.len());
            return entries;
        }
    }
    Vec::new()
}

pub(super) fn save_history(history: &[String]) {
    if let Ok(path) = get_history_cache_path() {
        let content = history.join("\n");
        if let Err(error) = fs::write(&path, content) {
            eprintln!("Warning: Failed to save history to {:?}: {}", path, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RaffiConfig;

    #[test]
    fn test_word_prefix_match() {
        assert!(word_prefix_match("Brave Browser", "brave"));
        assert!(word_prefix_match("Search Brave Search", "brave"));
        assert!(!word_prefix_match("Unbraveable", "brave"));
        assert!(word_prefix_match("cargo go go", "go go"), "overlapping");
        assert!(word_prefix_match("x---", "--"));
    }

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

        let results = fuzzy_match_configs(&configs, "Firefox", "Firefox");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], 0);

        let results = fuzzy_match_configs(&configs, "fox", "fox");
        assert!(results.contains(&0));

        let results = fuzzy_match_configs(&configs, "chr", "chr");
        assert!(results.contains(&1));

        let results = fuzzy_match_configs(&configs, "od", "od");
        assert!(results.contains(&3));

        let results = fuzzy_match_configs(&configs, "o", "o");
        assert!(results.len() >= 3);
        assert!(results.contains(&0));
        assert!(results.contains(&1));
        assert!(results.contains(&3));
    }

    #[test]
    fn test_try_evaluate_math_basic_operations() {
        let result = try_evaluate_math("2+2");
        assert!(result.is_some());
        assert_eq!(result.as_ref().unwrap().result, 4.0);

        let result = try_evaluate_math("10-3");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 7.0);

        let result = try_evaluate_math("5*6");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 30.0);

        let result = try_evaluate_math("20/4");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        let result = try_evaluate_math("2^3");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 8.0);

        let result = try_evaluate_math("17%5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 2.0);
    }

    #[test]
    fn test_try_evaluate_math_complex_expressions() {
        let result = try_evaluate_math("(10+5)*2");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 30.0);

        let result = try_evaluate_math("((2+3)*4)-5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 15.0);

        let result = try_evaluate_math("-5+10");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        let result = try_evaluate_math("3.5*2");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 7.0);
    }

    #[test]
    fn test_try_evaluate_math_functions() {
        let result = try_evaluate_math("sqrt(16)");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 4.0);

        let result = try_evaluate_math("abs(-5)");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        let result = try_evaluate_math("sin(0)");
        assert!(result.is_some());
        assert!((result.unwrap().result - 0.0).abs() < 0.0001);
    }

    #[test]
    fn test_try_evaluate_math_not_math() {
        assert!(try_evaluate_math("firefox").is_none());
        assert!(try_evaluate_math("google chrome").is_none());
        assert!(try_evaluate_math("hello world").is_none());
        assert!(try_evaluate_math("firefox123").is_none());
        assert!(try_evaluate_math("").is_none());
        assert!(try_evaluate_math("   ").is_none());
    }

    #[test]
    fn test_try_evaluate_math_invalid_expressions() {
        assert!(try_evaluate_math("1/0").is_none());
        assert!(try_evaluate_math("*5").is_none());
        assert!(try_evaluate_math("x+5").is_none());
        assert!(try_evaluate_math("2++2").is_none());
        assert!(try_evaluate_math("1 +").is_none());
    }

    /// Expressions documented on the website's calculator page.
    #[test]
    fn test_try_evaluate_math_documented_examples() {
        let cases = [
            ("5 + 3", 8.0),
            ("10 - 4", 6.0),
            ("7 * 6", 42.0),
            ("20 / 4", 5.0),
            ("17 % 5", 2.0),
            ("2 ^ 8", 256.0),
            ("2 ^ 10", 1024.0),
            ("sqrt(16)", 4.0),
            ("log(100)", 2.0),
            ("abs(-5)", 5.0),
            ("floor(3.7)", 3.0),
            ("ceil(3.2)", 4.0),
            ("cos(0)", 1.0),
            ("tan(0)", 0.0),
            ("sqrt(16) + abs(-5)", 9.0),
            ("2 + 3 * 4", 14.0),
            ("(2 + 3) * 4", 20.0),
            ("sqrt(16) * 5", 20.0),
            ("2 ^ 3 + sqrt(9)", 11.0),
            ("abs(-5) * floor(3.7)", 15.0),
            ("sqrt(144)", 12.0),
            ("floor(3.9)", 3.0),
            ("ceil(2.1)", 3.0),
            ("exp(0)", 1.0),
            ("ln(1)", 0.0),
        ];
        for (expression, expected) in cases {
            let result = try_evaluate_math(expression)
                .unwrap_or_else(|| panic!("no result for {expression}"));
            assert!(
                (result.result - expected).abs() < 1e-9,
                "{expression} = {}, expected {expected}",
                result.result
            );
        }

        let exp_ln = try_evaluate_math("exp(ln(10))").expect("exp(ln(10))");
        assert!((exp_ln.result - 10.0).abs() < 1e-9);
        let ln = try_evaluate_math("ln(2.718)").expect("ln(2.718)");
        assert!((ln.result - 1.0).abs() < 0.001);
        let exp = try_evaluate_math("exp(1)").expect("exp(1)");
        assert!((exp.result - std::f64::consts::E).abs() < 1e-9);
    }

    /// Semantics carried over from meval: float division, `pi`/`e`
    /// constants, scientific notation, and `^` as power rather than xor.
    #[test]
    fn test_try_evaluate_math_meval_semantics() {
        let half = try_evaluate_math("1/2").expect("1/2");
        assert!((half.result - 0.5).abs() < 1e-9);

        let quarter = try_evaluate_math("10/4").expect("10/4");
        assert!((quarter.result - 2.5).abs() < 1e-9);

        let pi = try_evaluate_math("2*pi").expect("2*pi");
        assert!((pi.result - 2.0 * std::f64::consts::PI).abs() < 1e-9);

        let e = try_evaluate_math("2*e").expect("2*e");
        assert!((e.result - 2.0 * std::f64::consts::E).abs() < 1e-9);

        let scientific = try_evaluate_math("1e3*2").expect("1e3*2");
        assert!((scientific.result - 2000.0).abs() < 1e-9);

        let negative_exponent = try_evaluate_math("1e-3+1").expect("1e-3+1");
        assert!((negative_exponent.result - 1.001).abs() < 1e-9);

        let big = try_evaluate_math("999999999999999999999 * 2");
        assert!(
            big.is_some(),
            "huge literals must not overflow integer math"
        );
    }
}
