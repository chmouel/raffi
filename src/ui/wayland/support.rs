use std::collections::HashMap;
use std::fs;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use super::state::CalculatorResult;
use crate::ui::{get_history_cache_path, get_mru_cache_path};
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

    match meval::eval_str(trimmed) {
        Ok(result) if result.is_finite() => Some(CalculatorResult {
            expression: trimmed.to_string(),
            result,
        }),
        _ => None,
    }
}

pub(super) fn fuzzy_match_configs(configs: &[RaffiConfig], query: &str) -> Vec<usize> {
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

    matches.sort_by(|a, b| b.1.cmp(&a.1));
    matches.into_iter().map(|(idx, _)| idx).collect()
}

pub(super) fn load_mru_map() -> HashMap<String, MruEntry> {
    if let Ok(path) = get_mru_cache_path() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut map = HashMap::new();
            for line in content.lines() {
                let parts: Vec<&str> = line.splitn(3, '|').collect();
                match parts.len() {
                    3 => {
                        // New format: desc|count|timestamp
                        if let (Ok(count), Ok(ts)) =
                            (parts[1].parse::<u32>(), parts[2].parse::<u64>())
                        {
                            map.insert(
                                parts[0].to_string(),
                                MruEntry {
                                    count,
                                    last_used: ts,
                                },
                            );
                        }
                    }
                    2 => {
                        // Old format: desc|count (auto-migrate, ts=0)
                        if let Ok(count) = parts[1].parse::<u32>() {
                            map.insert(
                                parts[0].to_string(),
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
            return map;
        }
    }
    HashMap::new()
}

pub(super) fn save_mru_map(map: &HashMap<String, MruEntry>) {
    if let Ok(path) = get_mru_cache_path() {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| b.1.count.cmp(&a.1.count));
        let content = entries
            .iter()
            .map(|(desc, entry)| format!("{}|{}|{}", desc, entry.count, entry.last_used))
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(error) = fs::write(&path, content) {
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

        let results = fuzzy_match_configs(&configs, "Firefox");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], 0);

        let results = fuzzy_match_configs(&configs, "fox");
        assert!(results.contains(&0));

        let results = fuzzy_match_configs(&configs, "chr");
        assert!(results.contains(&1));

        let results = fuzzy_match_configs(&configs, "od");
        assert!(results.contains(&3));

        let results = fuzzy_match_configs(&configs, "o");
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
    }
}
