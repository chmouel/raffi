//! Rows recently picked from the addons of `general.main_view`, listed
//! again when the query is empty.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::types::{ScriptFilterIcon, ScriptFilterItem};

/// Picks kept on disk; more than the main view shows, so closed tabs and
/// windows leave older picks to show instead.
const KEPT: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RecentPick {
    /// The `main_view` entry the row comes from, such as `browser_tabs` or
    /// `text_snippets.Snippets`.
    pub source: String,
    /// The row's identity, or the snippet name for snippets.
    pub key: String,
    pub last_used: u64,
    /// Page of a browser tab, to find it again under a new id once the
    /// browser restarted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Compositor instance a window was picked in, as window ids are
    /// reused by the next session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// A script filter row, which cannot be found again without running
    /// the script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<StoredItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredItem {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub variables: HashMap<String, String>,
}

impl StoredItem {
    pub fn of(item: &ScriptFilterItem, default_icon: Option<&str>) -> Self {
        Self {
            title: item.title.clone(),
            subtitle: item.subtitle.clone(),
            arg: item.arg.clone(),
            icon: item
                .icon
                .as_ref()
                .and_then(|icon| icon.path.clone())
                .or_else(|| default_icon.map(str::to_string)),
            variables: item.variables.clone(),
        }
    }

    pub fn to_item(&self) -> ScriptFilterItem {
        ScriptFilterItem {
            title: self.title.clone(),
            subtitle: self.subtitle.clone(),
            arg: self.arg.clone(),
            icon: self
                .icon
                .clone()
                .map(|path| ScriptFilterIcon { path: Some(path) }),
            preview: None,
            variables: self.variables.clone(),
            quicklookurl: None,
            text: None,
        }
    }
}

impl RecentPick {
    pub fn new(source: String, key: String) -> Self {
        Self {
            source,
            key,
            last_used: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            url: None,
            session: None,
            item: None,
        }
    }
}

/// Picks from JSON lines, newest first, skipping lines that do not parse.
pub(super) fn parse(content: &str) -> Vec<RecentPick> {
    let mut picks: Vec<RecentPick> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    // Stable, so picks made within the same second keep the file's order.
    picks.sort_by_key(|pick| std::cmp::Reverse(pick.last_used));
    let mut kept: Vec<RecentPick> = Vec::new();
    for pick in picks {
        if kept.len() == KEPT {
            break;
        }
        if !kept
            .iter()
            .any(|other| other.source == pick.source && other.key == pick.key)
        {
            kept.push(pick);
        }
    }
    kept
}

pub(super) fn load(path: Option<&Path>) -> Vec<RecentPick> {
    path.and_then(|path| fs::read_to_string(path).ok())
        .map(|content| parse(&content))
        .unwrap_or_default()
}

/// Put `pick` first, replacing an earlier pick of the same row.
pub(super) fn record(picks: &mut Vec<RecentPick>, pick: RecentPick) {
    picks.retain(|other| other.source != pick.source || other.key != pick.key);
    picks.insert(0, pick);
    picks.truncate(KEPT);
}

/// Write the picks readable by the user only, as they hold tab URLs and
/// script filter arguments.
pub(super) fn save(path: Option<&Path>, picks: &[RecentPick]) {
    let Some(path) = path else {
        return;
    };
    let content: String = picks
        .iter()
        .filter_map(|pick| serde_json::to_string(pick).ok())
        .map(|line| line + "\n")
        .collect();
    // Named after the process, so two Raffi instances never write the
    // same file; create_new also refuses a link left at that path.
    let temp = path.with_extension(format!("cache.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(content.as_bytes()))
        .and_then(|()| fs::rename(&temp, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        eprintln!("Warning: Failed to save recent picks to {path:?}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(source: &str, key: &str, last_used: u64) -> RecentPick {
        RecentPick {
            last_used,
            ..RecentPick::new(source.into(), key.into())
        }
    }

    #[test]
    fn parse_skips_bad_lines_dedupes_and_sorts_newest_first() {
        let lines = [
            serde_json::to_string(&pick("windows", "a", 10)).unwrap(),
            "not json".to_string(),
            serde_json::to_string(&pick("pass", "b", 30)).unwrap(),
            serde_json::to_string(&pick("windows", "a", 20)).unwrap(),
        ]
        .join("\n");
        let picks = parse(&lines);
        assert_eq!(
            picks
                .iter()
                .map(|pick| (pick.key.as_str(), pick.last_used))
                .collect::<Vec<_>>(),
            [("b", 30), ("a", 20)]
        );
    }

    #[test]
    fn record_moves_a_pick_first_and_caps_the_list() {
        let mut picks: Vec<_> = (0..KEPT)
            .map(|n| pick("windows", &n.to_string(), 0))
            .collect();
        record(&mut picks, pick("windows", "5", 1));
        assert_eq!(picks.len(), KEPT);
        assert_eq!(picks[0].key, "5");
        assert_eq!(picks.iter().filter(|pick| pick.key == "5").count(), 1);
        record(&mut picks, pick("pass", "new", 2));
        assert_eq!(picks.len(), KEPT);
        assert_eq!(picks[0].key, "new");
        assert_eq!(picks.last().unwrap().key, (KEPT - 2).to_string());
    }

    #[test]
    fn save_writes_a_private_file_that_loads_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("raffi-recent-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recent.cache");
        let mut tab = pick("browser_tabs", "1:2", 5);
        tab.url = Some("https://example.com".into());
        let picks = vec![tab, pick("pass", "entry:mail", 4)];
        save(Some(&path), &picks);
        assert_eq!(load(Some(&path)), picks);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }
}
