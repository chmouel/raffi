use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use super::state::EmojiEntry;

/// Maximum number of emoji results to render at once to keep the UI responsive.
const EMOJI_DISPLAY_LIMIT: usize = 50;

/// Base URL for downloading rofimoji CSV data files.
const ROFIMOJI_DATA_URL: &str =
    "https://raw.githubusercontent.com/fdw/rofimoji/main/src/picker/data";

/// Small built-in fallback used when no cached files exist and download fails.
static EMOJI_FALLBACK: &[(&str, &str)] = &[
    ("😀", "grinning face"),
    ("😂", "face with tears of joy"),
    ("🥰", "smiling face with hearts"),
    ("😍", "smiling face with heart-eyes"),
    ("😎", "smiling face with sunglasses"),
    ("🤔", "thinking face"),
    ("😢", "crying face"),
    ("😡", "pouting face"),
    ("👍", "thumbs up"),
    ("👎", "thumbs down"),
    ("👋", "waving hand"),
    ("🙏", "folded hands"),
    ("👏", "clapping hands"),
    ("💪", "flexed biceps"),
    ("❤️", "red heart"),
    ("🔥", "fire"),
    ("⭐", "star"),
    ("✨", "sparkles"),
    ("🎉", "party popper"),
    ("💯", "hundred points"),
    ("✅", "check mark button"),
    ("❌", "cross mark"),
    ("⚠️", "warning"),
    ("🚀", "rocket"),
    ("💡", "light bulb"),
    ("🔑", "key"),
    ("🔒", "locked"),
    ("📌", "pushpin"),
    ("📎", "paperclip"),
    ("📝", "memo"),
    ("📅", "calendar"),
    ("🕐", "one o'clock"),
    ("🐛", "bug"),
    ("🍕", "pizza"),
    ("☕", "hot beverage"),
    ("🏠", "house"),
    ("🎵", "musical note"),
    ("🔔", "bell"),
    ("💬", "speech balloon"),
    ("👀", "eyes"),
    ("🤝", "handshake"),
    ("🎯", "bullseye"),
    ("🏆", "trophy"),
    ("💎", "gem stone"),
    ("🌍", "globe showing Europe-Africa"),
    ("☀️", "sun"),
    ("🌙", "crescent moon"),
    ("⚡", "high voltage"),
    ("🔧", "wrench"),
    ("⚙️", "gear"),
];

pub(super) fn emoji_fallback_entries() -> Vec<EmojiEntry> {
    EMOJI_FALLBACK
        .iter()
        .map(|&(value, name)| EmojiEntry {
            value: value.to_string(),
            name: name.to_string(),
        })
        .collect()
}

fn emoji_cache_dir() -> String {
    format!(
        "{}/raffi/emoji",
        std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()))
    )
}

fn parse_emoji_csv(content: &str) -> Vec<EmojiEntry> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }

            let (value, rest) = line.split_once(' ')?;
            let name = if let Some(index) = rest.find(" <small>") {
                &rest[..index]
            } else {
                rest
            };

            Some(EmojiEntry {
                value: value.to_string(),
                name: name.to_string(),
            })
        })
        .collect()
}

pub(super) fn resolve_emoji_file_names(config: &crate::EmojiAddonConfig) -> Vec<String> {
    config.data_files.as_ref().cloned().unwrap_or_else(|| {
        crate::DEFAULT_EMOJI_FILES
            .iter()
            .map(|name| name.to_string())
            .collect()
    })
}

fn find_emoji_file_on_disk(name: &str) -> Option<PathBuf> {
    let cache = PathBuf::from(format!("{}/{name}.csv", emoji_cache_dir()));
    if cache.exists() {
        return Some(cache);
    }

    for prefix in ["/usr/lib", "/usr/local/lib"] {
        if let Ok(entries) = fs::read_dir(prefix) {
            for entry in entries.flatten() {
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if dir_name.starts_with("python3") {
                    let candidate = entry
                        .path()
                        .join("site-packages/picker/data")
                        .join(format!("{name}.csv"));
                    if candidate.exists() {
                        return Some(candidate);
                    }
                }
            }
        }
    }

    None
}

pub(super) fn load_emoji_data_from_disk(file_names: &[String]) -> Vec<EmojiEntry> {
    let mut entries = Vec::new();
    for name in file_names {
        if let Some(path) = find_emoji_file_on_disk(name) {
            match fs::read_to_string(&path) {
                Ok(content) => {
                    let before = entries.len();
                    entries.extend(parse_emoji_csv(&content));
                    crate::debug_log!(
                        "emoji: loaded {} entries from {}",
                        entries.len() - before,
                        path.display()
                    );
                }
                Err(error) => eprintln!("raffi: failed to read {}: {error}", path.display()),
            }
        }
    }
    crate::debug_log!("emoji: total {} entries from disk", entries.len());
    entries
}

pub(super) fn download_and_load_emoji_data(file_names: Vec<String>) -> Vec<EmojiEntry> {
    crate::debug_log!("emoji: downloading data for files: {file_names:?}");
    let dir = emoji_cache_dir();
    let _ = fs::create_dir_all(&dir);

    for name in &file_names {
        let dest = PathBuf::from(format!("{dir}/{name}.csv"));
        if !dest.exists() {
            if find_emoji_file_on_disk(name).is_some() {
                continue;
            }

            let url = format!("{ROFIMOJI_DATA_URL}/{name}.csv");
            if let Err(error) = download_to_file(&url, &dest) {
                eprintln!("raffi: failed to download {url}: {error}");
            }
        }
    }

    load_emoji_data_from_disk(&file_names)
}

fn download_to_file(url: &str, dest: &Path) -> anyhow::Result<()> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build();
    let agent: ureq::Agent = config.into();
    let mut body = agent.get(url).call()?.into_body().into_reader();
    let mut file = std::fs::File::create(dest)?;
    std::io::copy(&mut body, &mut file)?;
    Ok(())
}

pub(super) fn filter_emoji_into(
    data: &[EmojiEntry],
    query: &str,
    matcher: &SkimMatcherV2,
    out: &mut Vec<usize>,
) {
    out.clear();
    if query.is_empty() {
        out.extend(0..data.len().min(EMOJI_DISPLAY_LIMIT));
        return;
    }

    let mut scored: Vec<(usize, i64)> = data
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            matcher
                .fuzzy_match(&entry.name, query)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.truncate(EMOJI_DISPLAY_LIMIT);
    out.extend(scored.iter().map(|(index, _)| *index));
}

#[cfg(test)]
mod tests {
    use fuzzy_matcher::skim::SkimMatcherV2;

    use super::{
        emoji_cache_dir, emoji_fallback_entries, filter_emoji_into, find_emoji_file_on_disk,
        load_emoji_data_from_disk, parse_emoji_csv, resolve_emoji_file_names, EmojiEntry,
        EMOJI_DISPLAY_LIMIT,
    };

    fn test_emoji_data() -> Vec<EmojiEntry> {
        vec![
            EmojiEntry {
                value: "😀".into(),
                name: "grinning face".into(),
            },
            EmojiEntry {
                value: "😂".into(),
                name: "face with tears of joy".into(),
            },
            EmojiEntry {
                value: "🐴".into(),
                name: "horse face".into(),
            },
            EmojiEntry {
                value: "🏠".into(),
                name: "house".into(),
            },
            EmojiEntry {
                value: "\u{F015}".into(),
                name: "nf-fa: home".into(),
            },
        ]
    }

    #[test]
    fn test_parse_emoji_csv_basic() {
        let csv = "😀 grinning face\n😂 face with tears of joy\n";
        let entries = parse_emoji_csv(csv);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].value, "😀");
        assert_eq!(entries[0].name, "grinning face");
        assert_eq!(entries[1].value, "😂");
        assert_eq!(entries[1].name, "face with tears of joy");
    }

    #[test]
    fn test_parse_emoji_csv_strips_small_tags() {
        let csv = "😀 grinning face <small>(face, grin)</small>\n";
        let entries = parse_emoji_csv(csv);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "grinning face");
    }

    #[test]
    fn test_parse_emoji_csv_nerd_font_format() {
        let csv = "\u{EB99} cod-account\n\u{F015} fa-home\n";
        let entries = parse_emoji_csv(csv);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "cod-account");
        assert_eq!(entries[1].value, "\u{F015}");
        assert_eq!(entries[1].name, "fa-home");
    }

    #[test]
    fn test_parse_emoji_csv_empty_and_blank_lines() {
        let csv = "\n   \n😀 grinning face\n\n";
        let entries = parse_emoji_csv(csv);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_emoji_fallback_not_empty() {
        let fallback = emoji_fallback_entries();
        assert!(!fallback.is_empty());
        assert!(fallback.len() >= 40);
    }

    #[test]
    fn test_filter_emoji_empty_query_returns_all() {
        let data = test_emoji_data();
        let matcher = SkimMatcherV2::default();
        let mut out = Vec::new();
        filter_emoji_into(&data, "", &matcher, &mut out);
        assert_eq!(out.len(), data.len());
    }

    #[test]
    fn test_filter_emoji_matches_by_name() {
        let data = test_emoji_data();
        let matcher = SkimMatcherV2::default();
        let mut out = Vec::new();
        filter_emoji_into(&data, "grinning", &matcher, &mut out);
        assert!(!out.is_empty());
        for &index in &out {
            assert!(data[index].name.contains("grinning"));
        }
    }

    #[test]
    fn test_filter_emoji_fuzzy_matches() {
        let data = test_emoji_data();
        let matcher = SkimMatcherV2::default();
        let mut out = Vec::new();
        filter_emoji_into(&data, "hrse", &matcher, &mut out);
        assert!(!out.is_empty());
        let names: Vec<&str> = out.iter().map(|&index| data[index].name.as_str()).collect();
        assert!(names.iter().any(|name| name.contains("horse")));
    }

    #[test]
    fn test_filter_emoji_nf_icons() {
        let data = test_emoji_data();
        let matcher = SkimMatcherV2::default();
        let mut out = Vec::new();
        filter_emoji_into(&data, "nf-fa: home", &matcher, &mut out);
        assert!(!out.is_empty());
        assert_eq!(data[out[0]].name, "nf-fa: home");
        assert_eq!(data[out[0]].value, "\u{F015}");
    }

    #[test]
    fn test_filter_emoji_no_match_returns_empty() {
        let data = test_emoji_data();
        let matcher = SkimMatcherV2::default();
        let mut out = Vec::new();
        filter_emoji_into(&data, "zzzzzzzzzzzzzzzzz", &matcher, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn test_filter_emoji_caps_results() {
        let data: Vec<EmojiEntry> = (0..100)
            .map(|index| EmojiEntry {
                value: format!("e{index}"),
                name: format!("entry number {index}"),
            })
            .collect();
        let matcher = SkimMatcherV2::default();
        let mut out = Vec::new();
        filter_emoji_into(&data, "entry", &matcher, &mut out);
        assert!(out.len() <= EMOJI_DISPLAY_LIMIT);
    }

    #[test]
    fn test_find_emoji_file_on_disk_from_cache() {
        let dir = emoji_cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let test_name = "_test_find_disk_probe";
        let path = format!("{dir}/{test_name}.csv");
        std::fs::write(&path, "😀 test\n").unwrap();
        let found = find_emoji_file_on_disk(test_name);
        assert!(found.is_some());
        assert_eq!(found.unwrap().to_str().unwrap(), path);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_emoji_data_from_disk_parses_cached_files() {
        let dir = emoji_cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let test_name = "_test_load_disk_probe";
        let path = format!("{dir}/{test_name}.csv");
        std::fs::write(&path, "😀 grinning\n🔥 fire\n").unwrap();
        let entries = load_emoji_data_from_disk(&[test_name.to_string()]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].value, "😀");
        assert_eq!(entries[1].name, "fire");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_emoji_data_from_disk_missing_file_returns_empty() {
        let entries = load_emoji_data_from_disk(&["_nonexistent_file_xyz".to_string()]);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_resolve_emoji_file_names_default() {
        let config = crate::EmojiAddonConfig::default();
        let names = resolve_emoji_file_names(&config);
        assert_eq!(names.len(), crate::DEFAULT_EMOJI_FILES.len());
        assert_eq!(names[0], "emojis_smileys_emotion");
    }

    #[test]
    fn test_resolve_emoji_file_names_custom() {
        let config = crate::EmojiAddonConfig {
            data_files: Some(vec!["nerd_font".to_string(), "gitmoji".to_string()]),
            ..Default::default()
        };
        let names = resolve_emoji_file_names(&config);
        assert_eq!(names, vec!["nerd_font", "gitmoji"]);
    }
}
