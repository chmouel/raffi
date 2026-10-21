//! Browser history: read Firefox-style `places.sqlite` and Chromium
//! `History` databases, rank pages by frecency, and collapse the same URL
//! across profiles.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use rusqlite::{Connection, OpenFlags};

use crate::browser_host::{self, BrowserProfile};
use crate::{BookmarkBrowser, BookmarkOpener, BrowserHistoryAddonConfig};

use super::actions::DeferredAction;
use super::browser_data::{dir_name, opener, snapshot_database, Opener};
use super::command_list::{CommandAction, CommandItem, RowGroup, MATCH_FIELD_SEPARATOR};
use super::url_match::{base_domain, query_host, uri_host};

pub(super) const IDENTITY_PREFIX: &str = "history:";
const FALLBACK_ICON: &str = "web-browser";
const HISTORY_ICONS: [&str; 2] = ["document-open-recent", "web-browser"];
const MAX_LIBRARY_ENTRIES: usize = 50_000;
const CHROMIUM_EPOCH_OFFSET: i64 = 11_644_473_600_000_000;
const TRACKING_PARAMS: [&str; 9] = [
    "fbclid",
    "gclid",
    "dclid",
    "msclkid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "si",
    "utm_source",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Entry {
    /// Canonical URL used for identity and deduplication.
    pub canonical: String,
    /// Latest raw URL, which actions open.
    pub url: String,
    pub title: String,
    /// Lowercased URI host, precomputed for display and favicon lookup.
    pub host: String,
    /// Registrable domain of `host`, precomputed for `site:` and domain matching.
    pub domain: Option<String>,
    /// Unix seconds.
    pub last_visit: i64,
    pub visit_count: u64,
    pub typed_count: u64,
}

impl Entry {
    pub(super) fn new(
        canonical: String,
        url: String,
        title: String,
        last_visit: i64,
        visit_count: u64,
        typed_count: u64,
    ) -> Self {
        let host = uri_host(&url)
            .map(|host| host.to_ascii_lowercase())
            .unwrap_or_default();
        let domain = (!host.is_empty()).then(|| base_domain(&host));
        Self {
            canonical,
            url,
            title,
            host,
            domain,
            last_visit,
            visit_count,
            typed_count,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Source {
    pub key: String,
    pub label: String,
    pub icon: String,
    /// Browser names and program names matched by `browser:`.
    pub browser_names: Vec<String>,
    pub profile_name: String,
    pub profile: Option<PathBuf>,
    pub entries: Vec<Entry>,
    pub opener: Opener,
    /// Favicon databases, `true` marking Firefox-style ones.
    pub favicons: Vec<(PathBuf, bool)>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Library {
    pub sources: Vec<Source>,
    pub errors: Vec<String>,
    /// Site icons by [`super::favicons::host_key`].
    pub icons: HashMap<String, PathBuf>,
    /// Identities picked previously from `browser_history` in the main view.
    pub picked: HashSet<String>,
}

/// How the history list is ordered, switched with Ctrl+Tab.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum View {
    #[default]
    Recent,
    Frequent,
    ByBrowser,
}

impl View {
    pub(super) fn cycle(self, backwards: bool) -> Self {
        let views = [Self::Recent, Self::Frequent, Self::ByBrowser];
        let at = views.iter().position(|view| *view == self).unwrap_or(0);
        let next = if backwards {
            (at + views.len() - 1) % views.len()
        } else {
            (at + 1) % views.len()
        };
        views[next]
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Frequent => "Frequent",
            Self::ByBrowser => "By browser",
        }
    }
}

/// The browser history mode: the loaded library and what the rows show.
#[derive(Debug, Default)]
pub(super) struct BrowserHistoryState {
    /// The command list session the state belongs to.
    pub session: u64,
    /// The query after the keyword the rows show.
    pub query: String,
    pub library: Option<Arc<Library>>,
    pub view: View,
    /// The view and filters the rows were built for, without free text.
    pub built: Option<(View, Filters)>,
    /// The latest scheduled search and whether it is still waiting.
    pub search_generation: u64,
    pub search_pending: bool,
}

impl BrowserHistoryState {
    pub fn view_label(&self) -> String {
        self.view.label().to_string()
    }
}

/// What rows show and how actions behave.
#[derive(Debug, Clone, Copy)]
pub(super) struct RowOptions {
    pub keep_open: bool,
}

#[derive(Debug, Default)]
pub(super) struct Rows {
    pub items: Vec<CommandItem>,
    pub usage: HashMap<String, u64>,
}

/// Filters parsed from the query of the dedicated history list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Filters {
    site: Option<String>,
    browser: Option<String>,
    profile: Option<String>,
    /// Unix seconds; inclusive.
    after: Option<i64>,
    /// Unix seconds; exclusive.
    before: Option<i64>,
    /// The words left for fuzzy matching.
    pub text: String,
}

impl Filters {
    fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.site.is_none()
            && self.browser.is_none()
            && self.profile.is_none()
            && self.after.is_none()
            && self.before.is_none()
    }
}

/// Parse the filter words of `query`, leaving unrecognized words for fuzzy
/// matching.
pub(super) fn parse_filters(query: &str, now: i64) -> Filters {
    let mut filters = Filters::default();
    let mut words: Vec<&str> = Vec::new();
    for word in query.split_whitespace() {
        let Some((key, value)) = word.split_once(':') else {
            words.push(word);
            continue;
        };
        if value.is_empty() {
            words.push(word);
            continue;
        }
        match key.to_ascii_lowercase().as_str() {
            "site" => {
                let Some(host) = query_host(value) else {
                    words.push(word);
                    continue;
                };
                filters.site = Some(base_domain(&host));
            }
            "browser" => filters.browser = Some(normalize_name(value)),
            "profile" => filters.profile = Some(value.to_lowercase()),
            "after" => match parse_when(value, now) {
                Some(when) => filters.after = Some(when),
                None => words.push(word),
            },
            "before" => match parse_when(value, now) {
                Some(when) => filters.before = Some(when),
                None => words.push(word),
            },
            _ => words.push(word),
        }
    }
    filters.text = words.join(" ");
    filters
}

fn normalize_name(value: &str) -> String {
    value.trim().to_lowercase().replace([' ', '_'], "-")
}

fn parse_when(value: &str, now: i64) -> Option<i64> {
    let lower = value.to_lowercase();
    let today = DateTime::from_timestamp(now, 0)?
        .with_timezone(&Local)
        .date_naive();
    match lower.as_str() {
        "today" => return local_midnight(today),
        "yesterday" => {
            return local_midnight(today.pred_opt()?);
        }
        _ => {}
    }
    if let Some(days) = relative_days(&lower) {
        return Some(local_midnight(today)?.saturating_sub(days.saturating_mul(86_400)));
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    local_midnight(date)
}

fn relative_days(value: &str) -> Option<i64> {
    let (number, factor) = if let Some(n) = value.strip_suffix('d') {
        (n, 1_i64)
    } else if let Some(n) = value.strip_suffix('w') {
        (n, 7)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 30)
    } else {
        (value.strip_suffix('y')?, 365)
    };
    let days = number.parse::<i64>().ok()?.checked_mul(factor)?;
    (days >= 0).then_some(days)
}

fn local_midnight(date: NaiveDate) -> Option<i64> {
    let naive = date.and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|time| time.timestamp())
}

fn source_matches(filters: &Filters, source: &Source) -> bool {
    if let Some(browser) = &filters.browser {
        let matched = source
            .browser_names
            .iter()
            .any(|name| normalize_name(name) == *browser);
        if !matched {
            return false;
        }
    }
    if let Some(profile) = &filters.profile {
        let name = source.profile_name.to_lowercase();
        let dir = source
            .profile
            .as_deref()
            .map(dir_name)
            .unwrap_or_default()
            .to_lowercase();
        if !name.contains(profile) && !dir.contains(profile) {
            return false;
        }
    }
    true
}

fn entry_matches(filters: &Filters, entry: &Entry) -> bool {
    if let Some(site) = &filters.site {
        if entry.domain.as_deref() != Some(site.as_str()) {
            return false;
        }
    }
    if filters.after.is_some_and(|after| entry.last_visit < after) {
        return false;
    }
    if filters
        .before
        .is_some_and(|before| entry.last_visit >= before)
    {
        return false;
    }
    true
}

/// One URL as seen in one source.
#[derive(Debug, Clone)]
struct Occurrence<'a> {
    source: &'a Source,
    entry: &'a Entry,
}

#[derive(Debug)]
struct Merged<'a> {
    canonical: &'a str,
    occurrences: Vec<Occurrence<'a>>,
}

fn merged<'a>(library: &'a Library, filters: &Filters) -> Vec<Merged<'a>> {
    let mut order: Vec<&'a str> = Vec::new();
    let mut by_url: HashMap<&'a str, Vec<Occurrence<'a>>> = HashMap::new();
    for source in &library.sources {
        if !source_matches(filters, source) {
            continue;
        }
        for entry in &source.entries {
            if !entry_matches(filters, entry) {
                continue;
            }
            let canonical = entry.canonical.as_str();
            if !by_url.contains_key(canonical) {
                order.push(canonical);
            }
            by_url
                .entry(canonical)
                .or_default()
                .push(Occurrence { source, entry });
        }
    }
    order
        .into_iter()
        .filter_map(|canonical| {
            let occurrences = by_url.remove(canonical)?;
            Some(Merged {
                canonical,
                occurrences,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
struct Totals {
    last_visit: i64,
    visits: u64,
    typed: u64,
}

fn totals(merged: &Merged<'_>) -> Totals {
    let mut totals = Totals {
        last_visit: 0,
        visits: 0,
        typed: 0,
    };
    for occurrence in &merged.occurrences {
        totals.last_visit = totals.last_visit.max(occurrence.entry.last_visit);
        totals.visits = totals.visits.saturating_add(occurrence.entry.visit_count);
        totals.typed = totals.typed.saturating_add(occurrence.entry.typed_count);
    }
    totals
}

fn score(entry: &Totals, now: i64, max_visits: u64, max_typed: u64, picked: bool) -> f64 {
    let age_days = (now.saturating_sub(entry.last_visit) as f64 / 86_400.0).max(0.0);
    let recency = 0.5_f64.powf(age_days / 14.0);
    let frequency = if max_visits > 1 {
        (1.0 + entry.visits as f64).ln() / (1.0 + max_visits as f64).ln()
    } else {
        0.0
    };
    let typed = if max_typed > 1 {
        (1.0 + entry.typed as f64).ln() / (1.0 + max_typed as f64).ln()
    } else {
        0.0
    };
    let pick = if picked { 1.0 } else { 0.0 };
    0.45 * recency + 0.30 * frequency + 0.15 * typed + 0.10 * pick
}

fn latest_occurrence<'a>(occurrences: &'a [Occurrence<'a>]) -> &'a Occurrence<'a> {
    occurrences
        .iter()
        .max_by_key(|occurrence| occurrence.entry.last_visit)
        .expect("merged entries have an occurrence")
}

fn title(occurrences: &[Occurrence<'_>]) -> String {
    let latest = latest_occurrence(occurrences);
    occurrences
        .iter()
        .filter(|occurrence| !occurrence.entry.title.trim().is_empty())
        .max_by_key(|occurrence| occurrence.entry.last_visit)
        .map(|occurrence| occurrence.entry.title.clone())
        .unwrap_or_else(|| latest.entry.url.clone())
}

fn profiles(occurrences: &[Occurrence<'_>]) -> Vec<String> {
    let mut ordered: Vec<&Occurrence<'_>> = occurrences.iter().collect();
    ordered.sort_by_key(|occurrence| std::cmp::Reverse(occurrence.entry.last_visit));
    let mut labels = Vec::new();
    for occurrence in ordered {
        if !labels.contains(&occurrence.source.label) {
            labels.push(occurrence.source.label.clone());
        }
    }
    labels
}

fn format_visit(last_visit: i64, now: DateTime<Local>) -> (Option<String>, String) {
    let Some(then) = DateTime::from_timestamp(last_visit, 0).map(|t| t.with_timezone(&Local))
    else {
        return (None, String::new());
    };
    let days = (now.date_naive() - then.date_naive()).num_days();
    let badge = match days {
        d if d <= 0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        d if d < 7 => format!("{d}d"),
        d if d < 30 => format!("{}w", d / 7),
        d if d < 365 => format!("{}mo", d / 30),
        d => format!("{}y", d / 365),
    };
    (Some(badge), then.format("%Y-%m-%d %H:%M").to_string())
}

fn action(action: DeferredAction, options: RowOptions) -> CommandAction {
    CommandAction {
        action,
        confirm: None,
        keep_open: options.keep_open,
        progress: None,
    }
}

fn match_text(url: &str, title: &str, source: &Source) -> String {
    format!(
        "{url}{MATCH_FIELD_SEPARATOR}{title}{MATCH_FIELD_SEPARATOR}{} {}",
        source.label, source.profile_name
    )
}

fn item_for_merged(
    library: &Library,
    merged: &Merged<'_>,
    totals: Totals,
    options: RowOptions,
    now: DateTime<Local>,
) -> CommandItem {
    let latest = latest_occurrence(&merged.occurrences);
    let source = latest.source;
    let entry = latest.entry;
    let title = title(&merged.occurrences);
    let profiles = profiles(&merged.occurrences);
    let host = if entry.host.is_empty() {
        entry.url.as_str()
    } else {
        entry.host.as_str()
    };
    let mut subtitle = format!("{host} · {}", source.label);
    if profiles.len() > 1 {
        subtitle.push_str(&format!(" + {} more", profiles.len() - 1));
    }
    if totals.typed > 0 {
        subtitle.push_str(" · typed");
    }
    let (badge, last_visit_str) = format_visit(totals.last_visit, now);
    let mut details = vec![
        ("URL".to_string(), entry.url.clone()),
        ("Title".to_string(), title.clone()),
        ("Profiles".to_string(), profiles.join(", ")),
        ("Last visit".to_string(), last_visit_str),
        ("Visits".to_string(), totals.visits.to_string()),
    ];
    if totals.typed > 0 {
        details.push(("Typed".to_string(), totals.typed.to_string()));
    }
    details.push(("Ctrl+Tab".to_string(), "Switch history views".to_string()));
    let unavailable = match &source.opener {
        Opener::Argv { name, .. } => {
            details.push(("Enter".to_string(), format!("Open in {name}")));
            None
        }
        Opener::Missing(reason) => Some(reason.clone()),
    };
    details.push(("Ctrl+Enter".to_string(), "Copy the link".to_string()));
    let favicon = library
        .icons
        .get(&entry.host)
        .map(|path| path.to_string_lossy().into_owned());
    let icon_names = [source.icon.clone()]
        .into_iter()
        .chain(HISTORY_ICONS.map(str::to_string))
        .collect();
    let identity = format!("{IDENTITY_PREFIX}{}", merged.canonical);
    CommandItem {
        title,
        subtitle,
        match_text: match_text(&entry.url, &entry.title, source),
        icon_names,
        icon: favicon,
        identity,
        unavailable,
        primary: source.opener.action(&entry.url).map(|a| action(a, options)),
        secondary: Some(action(DeferredAction::Copy(entry.url.clone()), options)),
        details,
        badge,
        domains: entry.domain.iter().cloned().collect(),
        dimmed: false,
        sort_priority: 0,
        typed_only: false,
        group: None,
        pinned: false,
    }
}

fn item_for_source(
    library: &Library,
    source: &Source,
    entry: &Entry,
    totals: Totals,
    options: RowOptions,
    now: DateTime<Local>,
) -> CommandItem {
    let host = if entry.host.is_empty() {
        entry.url.as_str()
    } else {
        entry.host.as_str()
    };
    let mut subtitle = format!("{host} · {}", source.label);
    if totals.typed > 0 {
        subtitle.push_str(" · typed");
    }
    let (badge, last_visit_str) = format_visit(totals.last_visit, now);
    let mut details = vec![
        ("URL".to_string(), entry.url.clone()),
        ("Browser".to_string(), source.label.clone()),
        ("Profile".to_string(), source.profile_name.clone()),
        ("Last visit".to_string(), last_visit_str),
        ("Visits".to_string(), totals.visits.to_string()),
    ];
    if totals.typed > 0 {
        details.push(("Typed".to_string(), totals.typed.to_string()));
    }
    details.push(("Ctrl+Tab".to_string(), "Switch history views".to_string()));
    let unavailable = match &source.opener {
        Opener::Argv { name, .. } => {
            details.push(("Enter".to_string(), format!("Open in {name}")));
            None
        }
        Opener::Missing(reason) => Some(reason.clone()),
    };
    details.push(("Ctrl+Enter".to_string(), "Copy the link".to_string()));
    let favicon = library
        .icons
        .get(&entry.host)
        .map(|path| path.to_string_lossy().into_owned());
    let icon_names = [source.icon.clone()]
        .into_iter()
        .chain(HISTORY_ICONS.map(str::to_string))
        .collect();
    CommandItem {
        title: if entry.title.trim().is_empty() {
            entry.url.clone()
        } else {
            entry.title.clone()
        },
        subtitle,
        match_text: match_text(&entry.url, &entry.title, source),
        icon_names,
        icon: favicon,
        identity: format!("{IDENTITY_PREFIX}{}:{}", source.key, entry.canonical),
        unavailable,
        primary: source.opener.action(&entry.url).map(|a| action(a, options)),
        secondary: Some(action(DeferredAction::Copy(entry.url.clone()), options)),
        details,
        badge,
        domains: entry.domain.iter().cloned().collect(),
        dimmed: false,
        sort_priority: 0,
        typed_only: false,
        group: None,
        pinned: false,
    }
}

fn info_rows(library: &Library) -> Vec<CommandItem> {
    let mut rows = Vec::new();
    for (index, error) in library.errors.iter().enumerate() {
        let mut item = CommandItem::info(
            &format!("history-error:{index}"),
            error.clone(),
            "dialog-warning",
        );
        item.subtitle = "History source skipped".to_string();
        rows.push(item);
    }
    if library.sources.is_empty() && library.errors.is_empty() {
        rows.push(CommandItem::info(
            "history-none",
            "No browser history found".to_string(),
            "dialog-information",
        ));
    }
    rows
}

type SourceGroup<'a> = (&'a Source, Vec<(&'a Entry, Totals, f64)>);

/// The rows of `library` in `view`, with the ordering used to break fuzzy
/// ties.
pub(super) fn rows(library: &Library, view: View, filters: &Filters, options: RowOptions) -> Rows {
    let now = Local::now();
    let now_ts = now.timestamp();
    let merged = merged(library, filters);
    let mut max_visits = 0;
    let mut max_typed = 0;
    let mut all_totals = Vec::with_capacity(merged.len());
    for entry in &merged {
        let totals = totals(entry);
        max_visits = max_visits.max(totals.visits);
        max_typed = max_typed.max(totals.typed);
        all_totals.push(totals);
    }
    let mut rows = Rows::default();
    match view {
        View::Recent | View::Frequent => {
            rows.items = info_rows(library);
            let mut built: Vec<(f64, Totals, CommandItem)> = Vec::new();
            for (entry, totals) in merged.iter().zip(all_totals) {
                let identity = format!("{IDENTITY_PREFIX}{}", entry.canonical);
                let picked = library.picked.contains(&identity);
                let score = score(&totals, now_ts, max_visits, max_typed, picked);
                let item = item_for_merged(library, entry, totals, options, now);
                built.push((score, totals, item));
            }
            // `built` is already in view order. The command list keeps that
            // order for equal fuzzy scores, so no usage map is needed (which
            // also avoids sorting the whole list again on an empty query).
            if view == View::Recent {
                built.sort_by_key(|(_, totals, _)| std::cmp::Reverse(totals.last_visit));
            } else {
                built.sort_by(|a, b| {
                    b.0.total_cmp(&a.0)
                        .then(b.1.last_visit.cmp(&a.1.last_visit))
                });
            }
            rows.items
                .extend(built.into_iter().map(|(_, _, item)| item));
        }
        View::ByBrowser => {
            rows.items = info_rows(library);
            let mut groups: Vec<SourceGroup<'_>> = Vec::new();
            for source in &library.sources {
                if !source_matches(filters, source) {
                    continue;
                }
                let mut entries: Vec<(&Entry, Totals, f64)> = source
                    .entries
                    .iter()
                    .filter(|entry| entry_matches(filters, entry))
                    .map(|entry| {
                        let totals = Totals {
                            last_visit: entry.last_visit,
                            visits: entry.visit_count,
                            typed: entry.typed_count,
                        };
                        let identity =
                            format!("{IDENTITY_PREFIX}{}:{}", source.key, entry.canonical);
                        let picked = library.picked.contains(&identity);
                        let score = score(&totals, now_ts, max_visits, max_typed, picked);
                        (entry, totals, score)
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                entries.sort_by(|a, b| {
                    b.0.last_visit
                        .cmp(&a.0.last_visit)
                        .then(b.2.total_cmp(&a.2))
                });
                groups.push((source, entries));
            }
            groups.sort_by_key(|(_, entries)| std::cmp::Reverse(entries[0].0.last_visit));
            for (source, entries) in groups {
                let group = RowGroup::new(&source.label, Some(source.icon.as_str()));
                for (entry, totals, _score) in entries {
                    let mut item = item_for_source(library, source, entry, totals, options, now);
                    item.group = Some(group.clone());
                    rows.items.push(item);
                }
            }
        }
    }
    if rows.items.iter().all(|item| item.primary.is_none()) && !filters.is_empty() {
        rows.items.push(CommandItem::info(
            "history-no-match",
            "No history matches these filters".to_string(),
            "dialog-information",
        ));
    }
    rows
}

/// Every recent page, for the main view.
pub(super) fn flat_items(library: &Library, options: RowOptions) -> Vec<CommandItem> {
    rows(library, View::Recent, &Filters::default(), options).items
}

/// Look up the site icons of `library`; blocking, run it off the UI thread.
pub(super) fn lookup_icons(library: &Library) -> HashMap<String, PathBuf> {
    super::favicons::lookup_sites(
        library
            .sources
            .iter()
            .map(|source| {
                let mut hosts = BTreeSet::new();
                hosts.extend(
                    source
                        .entries
                        .iter()
                        .filter_map(|entry| super::favicons::host_key(&entry.url)),
                );
                (hosts, source.favicons.clone())
            })
            .collect(),
    )
}

/// Read every configured source, or every browser profile found. Blocking:
/// run it off the UI thread.
pub(super) fn load(config: &BrowserHistoryAddonConfig) -> Library {
    let Ok((home, config_dir)) = browser_host::user_dirs() else {
        return Library {
            errors: vec!["HOME is not set".to_string()],
            ..Library::default()
        };
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut library = load_from(config, &home, &config_dir, &path);
    if let Ok(cache) = crate::ui::get_recent_cache_path() {
        library.picked = super::recent::load(Some(&cache))
            .into_iter()
            .filter(|pick| pick.source == "browser_history")
            .map(|pick| pick.key)
            .collect();
    }
    library
}

struct Spec {
    browser: Option<BookmarkBrowser>,
    file: PathBuf,
    profile: Option<PathBuf>,
    profile_name: String,
    name: Option<String>,
    open_with: Option<BookmarkOpener>,
}

fn history_file(dir: &Path, gecko: bool) -> PathBuf {
    dir.join(if gecko { "places.sqlite" } else { "History" })
}

fn profile_spec(profile: BrowserProfile) -> Spec {
    let gecko = browser_host::browser_details(profile.browser).is_some_and(|(_, _, gecko)| gecko);
    Spec {
        browser: Some(profile.browser),
        file: history_file(&profile.dir, gecko),
        profile: Some(profile.dir),
        profile_name: profile.name,
        name: None,
        open_with: None,
    }
}

pub(super) fn load_from(
    config: &BrowserHistoryAddonConfig,
    home: &Path,
    config_dir: &Path,
    path: &OsStr,
) -> Library {
    let mut library = Library::default();
    let mut specs: Vec<Spec> = Vec::new();
    match &config.sources {
        None => specs.extend(
            browser_host::history_profiles(None, home, config_dir)
                .into_iter()
                .map(profile_spec),
        ),
        Some(sources) => {
            for source in sources {
                let with = |spec: Spec| Spec {
                    name: source.name.clone(),
                    open_with: source.open_with.clone(),
                    ..spec
                };
                if let Some(file) = &source.path {
                    let file = PathBuf::from(file);
                    let (file, profile) = if file.is_dir() {
                        let gecko = source
                            .browser
                            .and_then(browser_host::browser_details)
                            .is_some_and(|(_, _, gecko)| gecko);
                        let places = history_file(&file, true);
                        let chromium = history_file(&file, false);
                        let selected = match source.browser {
                            Some(_) => history_file(&file, gecko),
                            None if places.is_file() => places,
                            None => chromium,
                        };
                        (selected, file)
                    } else {
                        let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
                        (file, dir)
                    };
                    specs.push(with(Spec {
                        browser: source.browser,
                        file,
                        profile_name: dir_name(&profile),
                        profile: Some(profile),
                        name: None,
                        open_with: None,
                    }));
                    continue;
                }
                let Some(browser) = source.browser else {
                    library
                        .errors
                        .push("A browser_history source needs a browser or a path".to_string());
                    continue;
                };
                let browser_name =
                    browser_host::browser_details(browser).map_or("Browser", |(name, _, _)| name);
                let profiles: Vec<BrowserProfile> =
                    browser_host::history_profiles(Some(browser), home, config_dir)
                        .into_iter()
                        .filter(|profile| {
                            source.profile.as_ref().is_none_or(|wanted| {
                                profile.name.eq_ignore_ascii_case(wanted)
                                    || dir_name(&profile.dir) == *wanted
                            })
                        })
                        .collect();
                if profiles.is_empty() {
                    library.errors.push(match &source.profile {
                        Some(profile) => {
                            format!("{browser_name}: no profile {profile} with history")
                        }
                        None => format!("{browser_name}: no profile with history"),
                    });
                }
                specs.extend(profiles.into_iter().map(profile_spec).map(with));
            }
        }
    }

    let mut sources: Vec<(Source, String)> = Vec::new();
    let mut total = 0usize;
    for spec in specs {
        if total >= MAX_LIBRARY_ENTRIES {
            break;
        }
        let label = spec.name.clone().unwrap_or_else(|| {
            spec.browser
                .and_then(browser_host::browser_details)
                .map_or_else(|| dir_name(&spec.file), |(name, _, _)| name.to_string())
        });
        match read_source(&spec, config.open_with.as_ref(), path, config.max_entries()) {
            Ok(mut source) => {
                let take = source.entries.len().min(MAX_LIBRARY_ENTRIES - total);
                source.entries.truncate(take);
                total += source.entries.len();
                sources.push((Source { label, ..source }, spec.profile_name));
            }
            Err(error) => library.errors.push(format!("{label}: {error}")),
        }
    }
    let duplicated = |sources: &[(Source, String)], label: &str| {
        sources
            .iter()
            .filter(|(source, _)| source.label == label)
            .count()
            > 1
    };
    let labels: Vec<String> = sources
        .iter()
        .map(|(source, profile)| {
            if duplicated(&sources, &source.label) && !profile.is_empty() {
                format!("{} {profile}", source.label)
            } else {
                source.label.clone()
            }
        })
        .collect();
    for ((source, _), label) in sources.iter_mut().zip(labels) {
        source.label = label;
    }
    let mut keys: Vec<String> = Vec::new();
    for (mut source, _) in sources {
        let mut key = source.key.clone();
        let mut n = 1;
        while keys.contains(&key) {
            n += 1;
            key = format!("{}#{n}", source.key);
        }
        keys.push(key.clone());
        source.key = key;
        library.sources.push(source);
    }
    library
}

fn read_source(
    spec: &Spec,
    default_opener: Option<&BookmarkOpener>,
    path: &OsStr,
    max_entries: usize,
) -> Result<Source, String> {
    let entries = read_database(&spec.file, max_entries)?;
    let details = spec.browser.and_then(browser_host::browser_details);
    let icon = details
        .map_or(FALLBACK_ICON, |(_, icon, _)| icon)
        .to_string();
    let browser_names = spec
        .browser
        .map(|browser| {
            let mut names = browser_host::browser_window_apps(browser);
            if let Some((name, icon, _)) = browser_host::browser_details(browser) {
                names.push(name.to_string());
                names.push(icon.to_string());
            }
            names.push(format!("{browser:?}"));
            names
        })
        .unwrap_or_default();
    let mut favicons = Vec::new();
    for dir in spec.profile.iter() {
        let gecko = dir.join("favicons.sqlite");
        if gecko.is_file() {
            favicons.push((gecko, true));
        }
        let chromium = dir.join("Favicons");
        if chromium.is_file() {
            favicons.push((chromium, false));
        }
    }
    let key = format!(
        "{}:{}",
        spec.browser.map_or_else(
            || "file".to_string(),
            |browser| format!("{browser:?}").to_lowercase()
        ),
        spec.profile.as_deref().map(dir_name).unwrap_or_default()
    );
    Ok(Source {
        key,
        label: String::new(),
        icon,
        browser_names,
        profile_name: spec.profile_name.clone(),
        profile: spec.profile.clone(),
        entries,
        opener: opener(
            spec.open_with.as_ref().or(default_opener),
            spec.browser,
            spec.profile.as_deref(),
            path,
        ),
        favicons,
    })
}

fn read_database(file: &Path, max_entries: usize) -> Result<Vec<Entry>, String> {
    let (_snapshot, target) = snapshot_database(file, "history")?;
    let connection = Connection::open_with_flags(
        &target,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("cannot open {}: {error}", file.display()))?;
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
    if table_exists(&connection, "moz_places") {
        read_firefox(&connection, max_entries)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))
    } else if table_exists(&connection, "urls") {
        read_chromium(&connection, max_entries)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))
    } else {
        Err(format!(
            "{} is not a browser history database",
            file.display()
        ))
    }
}

fn table_exists(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
}

fn read_firefox(connection: &Connection, max_entries: usize) -> rusqlite::Result<Vec<Entry>> {
    let sql = "SELECT url, COALESCE(title, ''), COALESCE(visit_count, 0), \
               COALESCE(typed, 0), COALESCE(last_visit_date, 0) \
               FROM moz_places \
               WHERE COALESCE(hidden, 0) = 0 AND visit_count > 0 AND last_visit_date IS NOT NULL \
               AND (url LIKE 'http://%' OR url LIKE 'https://%') \
               ORDER BY {order} DESC LIMIT ?1";
    let mut entries = HashMap::new();
    for order in ["last_visit_date", "visit_count"] {
        let mut statement = connection.prepare(&sql.replace("{order}", order))?;
        let rows = statement.query_map([max_entries as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?.max(0) / 1_000_000,
            ))
        })?;
        for row in rows {
            let (url, title, visits, typed, last_visit) = row?;
            merge_source_entry(
                &mut entries,
                url,
                title,
                last_visit,
                visits.max(0) as u64,
                typed.max(0) as u64,
            );
        }
    }
    Ok(finish_entries(entries))
}

fn read_chromium(connection: &Connection, max_entries: usize) -> rusqlite::Result<Vec<Entry>> {
    let sql = "SELECT url, COALESCE(title, ''), COALESCE(visit_count, 0), \
               COALESCE(typed_count, 0), COALESCE(last_visit_time, 0) \
               FROM urls \
               WHERE COALESCE(hidden, 0) = 0 AND visit_count > 0 AND last_visit_time > 0 \
               AND (url LIKE 'http://%' OR url LIKE 'https://%') \
               ORDER BY {order} DESC LIMIT ?1";
    let mut entries = HashMap::new();
    for order in ["last_visit_time", "visit_count"] {
        let mut statement = connection.prepare(&sql.replace("{order}", order))?;
        let rows = statement.query_map([max_entries as i64], |row| {
            let last: i64 = row.get(4)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                last.saturating_sub(CHROMIUM_EPOCH_OFFSET).max(0) / 1_000_000,
            ))
        })?;
        for row in rows {
            let (url, title, visits, typed, last_visit) = row?;
            merge_source_entry(
                &mut entries,
                url,
                title,
                last_visit,
                visits.max(0) as u64,
                typed.max(0) as u64,
            );
        }
    }
    Ok(finish_entries(entries))
}

fn finish_entries(entries: HashMap<String, Entry>) -> Vec<Entry> {
    let mut list: Vec<Entry> = entries.into_values().collect();
    list.sort_by(|a, b| {
        b.last_visit
            .cmp(&a.last_visit)
            .then(b.visit_count.cmp(&a.visit_count))
            .then(a.canonical.cmp(&b.canonical))
    });
    list
}

fn merge_source_entry(
    entries: &mut HashMap<String, Entry>,
    url: String,
    title: String,
    last_visit: i64,
    visit_count: u64,
    typed_count: u64,
) {
    let Some(canonical) = canonical_url(&url) else {
        return;
    };
    match entries.get_mut(&canonical) {
        Some(existing) => {
            // The recent and frequent queries return the same row; keep the
            // larger counters instead of counting it twice.
            existing.visit_count = existing.visit_count.max(visit_count);
            existing.typed_count = existing.typed_count.max(typed_count);
            if last_visit >= existing.last_visit {
                existing.last_visit = last_visit;
                existing.url = url;
                if !title.trim().is_empty() {
                    existing.title = title;
                }
            }
        }
        None => {
            let entry = Entry::new(
                canonical.clone(),
                url,
                title,
                last_visit,
                visit_count,
                typed_count,
            );
            entries.insert(canonical, entry);
        }
    }
}

/// The canonical form used for identity and deduplication.
pub(super) fn canonical_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (before_fragment, fragment) = rest
        .split_once('#')
        .map_or((rest, None), |(a, b)| (a, Some(b)));
    let (before_query, query) = before_fragment
        .split_once('?')
        .map_or((before_fragment, None), |(a, b)| (a, Some(b)));
    let (authority, path) = before_query
        .split_once('/')
        .map_or((before_query, ""), |(a, b)| (a, b));
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let authority = authority.to_ascii_lowercase();
    let authority = match (scheme.as_str(), authority.rsplit_once(':')) {
        ("http", Some((host, "80"))) | ("https", Some((host, "443"))) => host.to_string(),
        _ => authority,
    };
    if authority.is_empty() {
        return None;
    }
    let mut path = format!("/{path}");
    if path.len() > 1 {
        while path.ends_with('/') {
            path.pop();
        }
    }
    let query = query.map(|query| {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|part| {
                let key = part
                    .split_once('=')
                    .map_or(*part, |(key, _)| key)
                    .to_lowercase();
                key != "si" && !key.starts_with("utm_") && !TRACKING_PARAMS.contains(&key.as_str())
            })
            .collect();
        kept.join("&")
    });
    let mut canonical = format!("{scheme}://{authority}{path}");
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        canonical.push('?');
        canonical.push_str(&query);
    }
    if let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) {
        if !fragment.starts_with(":~:text=") {
            canonical.push('#');
            canonical.push_str(fragment);
        }
    }
    Some(canonical)
}

#[cfg(test)]
pub(super) fn fixture_library() -> Library {
    let source = |key: &str, label: &str, entries: Vec<Entry>| Source {
        key: key.to_string(),
        label: label.to_string(),
        icon: label.to_lowercase(),
        browser_names: vec![label.to_lowercase(), label.to_string()],
        profile_name: "default".to_string(),
        profile: None,
        entries,
        opener: Opener::Argv {
            argv: vec!["firefox".to_string(), "{url}".to_string()],
            name: "Firefox".to_string(),
            apps: Vec::new(),
        },
        favicons: Vec::new(),
    };
    let entry = |url: &str, title: &str, last: i64, visits: u64, typed: u64| {
        Entry::new(
            canonical_url(url).unwrap(),
            url.to_string(),
            title.to_string(),
            last,
            visits,
            typed,
        )
    };
    Library {
        sources: vec![
            source(
                "firefox:default",
                "Firefox",
                vec![
                    entry(
                        "https://github.com/chmouel/raffi",
                        "Raffi",
                        1_700_000_000,
                        12,
                        3,
                    ),
                    entry(
                        "https://doc.rust-lang.org/std/",
                        "Rust docs",
                        1_699_000_000,
                        4,
                        0,
                    ),
                ],
            ),
            source(
                "brave:Default",
                "Brave",
                vec![entry(
                    "https://github.com/chmouel/raffi?utm_source=news",
                    "Raffi on GitHub",
                    1_700_000_500,
                    7,
                    1,
                )],
            ),
        ],
        ..Library::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> RowOptions {
        RowOptions { keep_open: false }
    }

    fn temporary(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("raffi-history-{name}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_canonical_url_normalizes_and_keeps_hash_routes() {
        assert_eq!(
            canonical_url("HTTPS://User@Example.COM:443/a/b/?utm_source=x&q=1#:~:text=hi")
                .as_deref(),
            Some("https://example.com/a/b?q=1")
        );
        assert_eq!(
            canonical_url("https://example.com/#/route").as_deref(),
            Some("https://example.com/#/route")
        );
        assert_eq!(
            canonical_url("http://example.com:8080/").as_deref(),
            Some("http://example.com:8080/")
        );
        assert_eq!(canonical_url("about:newtab"), None);
        assert_eq!(canonical_url("file:///tmp/a"), None);
    }

    #[test]
    fn test_parse_filters_keeps_invalid_tokens_as_text() {
        let now = 1_700_000_000;
        let today = DateTime::from_timestamp(now, 0)
            .unwrap()
            .with_timezone(&Local)
            .date_naive();
        let expected_after = local_midnight(today).unwrap() - 7 * 86_400;
        let filters = parse_filters(
            "raffi site:github.com browser:Brave profile:default after:7d before:2030-01-01 weird:x after:7ä before:999999999999999999y",
            now,
        );
        assert_eq!(filters.site.as_deref(), Some("github.com"));
        assert_eq!(filters.browser.as_deref(), Some("brave"));
        assert_eq!(filters.profile.as_deref(), Some("default"));
        assert_eq!(filters.after, Some(expected_after));
        assert!(filters.before.is_some());
        assert_eq!(
            filters.text,
            "raffi weird:x after:7ä before:999999999999999999y"
        );
        // Relative filters stay stable across seconds within the same local day.
        let later = parse_filters("after:7d", expected_after + 7 * 86_400 + 42);
        assert_eq!(later.after, Some(expected_after));
    }

    #[test]
    fn test_merged_history_sums_visits_and_uses_latest_opener() {
        let library = fixture_library();
        let rows = rows(&library, View::Recent, &Filters::default(), options());
        let raffi = rows
            .items
            .iter()
            .find(|item| item.identity == "history:https://github.com/chmouel/raffi")
            .unwrap();
        assert!(raffi
            .details
            .iter()
            .any(|(key, value)| key == "Visits" && value == "19"));
        assert!(raffi
            .details
            .iter()
            .any(|(key, value)| key == "Profiles" && value == "Brave, Firefox"));
        assert!(raffi.badge.is_some());
    }

    #[test]
    fn test_recent_and_frequent_orders_differ() {
        let mut library = fixture_library();
        library.sources[0].entries.push(Entry::new(
            canonical_url("https://old.example/").unwrap(),
            "https://old.example/".to_string(),
            "Old".to_string(),
            1_500_000_000,
            500,
            50,
        ));
        let recent = rows(&library, View::Recent, &Filters::default(), options());
        let frequent = rows(&library, View::Frequent, &Filters::default(), options());
        assert_eq!(
            recent.items[0].identity,
            "history:https://github.com/chmouel/raffi"
        );
        assert_eq!(frequent.items[0].identity, "history:https://old.example/");
    }

    #[test]
    fn test_filters_match_profile_and_site() {
        let library = fixture_library();
        let filters = parse_filters("site:github.com browser:firefox", 1_700_000_000);
        let rows = rows(&library, View::Recent, &filters, options());
        assert_eq!(rows.items.len(), 1);
        assert_eq!(
            rows.items[0].identity,
            "history:https://github.com/chmouel/raffi"
        );
        assert!(rows.items[0].subtitle.contains("Firefox"));
    }

    #[test]
    fn test_by_browser_groups_each_profile() {
        let library = fixture_library();
        let rows = rows(&library, View::ByBrowser, &Filters::default(), options());
        assert!(rows
            .items
            .iter()
            .any(|item| item.identity.starts_with("history:firefox:default:")));
        assert!(rows
            .items
            .iter()
            .any(|item| item.identity.starts_with("history:brave:Default:")));
        assert!(rows.items.iter().any(|item| item.group.is_some()));
    }

    #[test]
    fn test_reading_firefox_history_skips_hidden_and_internal() {
        let dir = temporary("firefox");
        let file = dir.join("places.sqlite");
        let connection = Connection::open(&file).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE moz_places (
                    url TEXT, title TEXT, visit_count INTEGER, typed INTEGER,
                    last_visit_date INTEGER, hidden INTEGER);
                 INSERT INTO moz_places VALUES
                    ('https://example.com/a', 'A', 3, 1, 1700000000000000, 0),
                    ('https://example.com/b', 'B', 2, 0, 1690000000000000, 1),
                    ('about:newtab', 'New tab', 5, 0, 1700000000000000, 0);",
            )
            .unwrap();
        drop(connection);
        let entries = read_database(&file, 100).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].canonical, "https://example.com/a");
        assert_eq!(entries[0].last_visit, 1_700_000_000);
        assert_eq!(entries[0].visit_count, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_reading_chromium_history_converts_epoch() {
        let dir = temporary("chromium");
        let file = dir.join("History");
        let connection = Connection::open(&file).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE urls (
                    url TEXT, title TEXT, visit_count INTEGER, typed_count INTEGER,
                    last_visit_time INTEGER, hidden INTEGER);
                 INSERT INTO urls VALUES
                    ('https://example.com/a', 'A', 3, 2, 13350000000000000, 0);",
            )
            .unwrap();
        drop(connection);
        let entries = read_database(&file, 100).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].last_visit, 1_705_526_400);
        assert_eq!(entries[0].typed_count, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_limit_keeps_the_most_recent_and_most_visited() {
        let dir = temporary("limit");
        let file = dir.join("History");
        let connection = Connection::open(&file).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE urls (
                    url TEXT, title TEXT, visit_count INTEGER, typed_count INTEGER,
                    last_visit_time INTEGER, hidden INTEGER);",
            )
            .unwrap();
        for index in 0..10 {
            connection
                .execute(
                    "INSERT INTO urls VALUES (?1, 'page', ?2, 0, ?3, 0)",
                    rusqlite::params![
                        format!("https://example.com/{index}"),
                        10 - index,
                        13350000000000000_i64 + index as i64
                    ],
                )
                .unwrap();
        }
        drop(connection);
        let entries = read_database(&file, 3).unwrap();
        assert_eq!(entries.len(), 6);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn perf_library(size: usize) -> Library {
        let entries: Vec<Entry> = (0..size)
            .map(|index| {
                let url = format!(
                    "https://site{}.example.com/path/{index}?utm_source=perf&id={index}",
                    index % 500
                );
                Entry::new(
                    canonical_url(&url).unwrap(),
                    url,
                    format!("Page {index} with a reasonably long browser title"),
                    1_700_000_000 - (index as i64 % 100_000),
                    (index as u64 % 40) + 1,
                    (index as u64 % 7) / 3,
                )
            })
            .collect();
        Library {
            sources: vec![Source {
                key: "perf:default".to_string(),
                label: "Firefox".to_string(),
                icon: "firefox".to_string(),
                browser_names: vec!["firefox".to_string()],
                profile_name: "default".to_string(),
                profile: None,
                entries,
                opener: Opener::Argv {
                    argv: vec!["firefox".to_string(), "{url}".to_string()],
                    name: "Firefox".to_string(),
                    apps: Vec::new(),
                },
                favicons: Vec::new(),
            }],
            ..Library::default()
        }
    }

    #[test]
    #[ignore = "performance baseline; run cargo test --release perf_history_search -- --ignored --nocapture"]
    fn perf_history_search() {
        use fuzzy_matcher::skim::SkimMatcherV2;
        use std::hint::black_box;
        use std::time::Instant;

        let matcher = SkimMatcherV2::default();
        for size in [2_000, 10_000, 50_000] {
            let library = perf_library(size);
            let started = Instant::now();
            let built = rows(&library, View::Recent, &Filters::default(), options());
            let row_time = started.elapsed();
            for query in ["", "r", "raffi", "site42", "zzzzzz"] {
                let mut out = Vec::new();
                let started = Instant::now();
                super::super::command_list::filter_items_into(
                    &built.items,
                    query,
                    &matcher,
                    &built.usage,
                    false,
                    super::super::command_list::MatchMode::Substring,
                    &mut out,
                );
                black_box(&out);
                println!(
                    "history size={size:>6} rows={row_time:>10?} query={query:?} filter={:>10?} matches={}",
                    started.elapsed(),
                    out.len()
                );
            }
        }
    }

    #[test]
    fn test_match_text_puts_the_url_before_the_title() {
        let rows = rows(
            &fixture_library(),
            View::Recent,
            &Filters::default(),
            options(),
        );
        let item = rows
            .items
            .iter()
            .find(|item| item.title == "Raffi on GitHub")
            .unwrap();
        let fields: Vec<&str> = item.match_text.split(MATCH_FIELD_SEPARATOR).collect();
        assert_eq!(
            fields[0],
            "https://github.com/chmouel/raffi?utm_source=news"
        );
        assert_eq!(fields[1], "Raffi on GitHub");
    }

    #[test]
    fn test_empty_and_error_rows() {
        let mut library = Library::default();
        library.errors.push("Firefox: no profile".to_string());
        let with_error = rows(&library, View::Recent, &Filters::default(), options());
        assert!(with_error
            .items
            .iter()
            .any(|item| item.identity == "history-error:0"));
        let empty = rows(
            &Library::default(),
            View::Recent,
            &Filters::default(),
            options(),
        );
        assert!(empty
            .items
            .iter()
            .any(|item| item.identity == "history-none"));
    }
}
