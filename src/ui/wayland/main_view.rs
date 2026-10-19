//! Addon results mixed into the main view (`general.main_view`), so typing
//! `gmail` lists matching browser tabs without the `tabs` trigger.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use iced::widget::operation::move_cursor_to_end;
use iced::Task;

use super::command_list::{CommandItem, CommandListAddon, CommandListState};
use super::recent::RecentPick;
use super::snippets::{
    compute_previews, fetch_clipboard, filter_snippets, load_snippet_source, missing_clipboard,
    snippet_mru_key,
};
use super::state::{ClipboardData, ClipboardSnapshot, LauncherApp, ScriptFilterResult};
use super::support::{contains_words, mru_bounds, mru_sort_key};
use super::types::Message;
use crate::{AddonsConfig, TextSnippet};

/// Most rows a single addon source adds to the main view.
pub(super) const MAX_ROWS: usize = 5;
/// Pause in typing before a script filter runs for the main view.
pub(super) const SCRIPT_FILTER_DEBOUNCE: Duration = Duration::from_millis(150);
/// Longest a main view script filter run may take, since it runs unasked.
const SCRIPT_FILTER_TIMEOUT: Duration = Duration::from_secs(10);
const SCRIPT_FILTER_MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// Set in the session of main view command lists, which also carries the
/// source index, so their favicon and tab preview results find their way
/// back and never match the keyword list.
pub(super) const MAIN_VIEW_SESSION: u64 = 1 << 63;

/// A resolved `main_view` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Entry {
    Launchers,
    List(CommandListAddon),
    /// Text snippet source by config index.
    Snippets(usize),
    /// Script filter by config index.
    ScriptFilter(usize),
}

/// Resolve `main_view` entries in order, warning about and skipping the ones
/// naming an unknown or disabled addon.
pub(super) fn resolve(entries: &[String], addons: &AddonsConfig) -> Vec<Entry> {
    let mut resolved = Vec::new();
    for raw in entries {
        let name = raw.trim();
        let name = name.strip_prefix("addons.").unwrap_or(name);
        let list = |enabled: bool, addon: CommandListAddon| {
            if enabled {
                Ok(Entry::List(addon))
            } else {
                Err(format!("main_view entry '{raw}' names a disabled addon"))
            }
        };
        let entry = match name {
            "launchers" => Ok(Entry::Launchers),
            "system" => list(addons.system.enabled, CommandListAddon::System),
            "windows" => list(addons.windows.enabled, CommandListAddon::Windows),
            "browser_tabs" => list(addons.browser_tabs.enabled, CommandListAddon::BrowserTabs),
            "media" => list(addons.media.enabled, CommandListAddon::Media),
            "audio" => list(addons.audio.enabled, CommandListAddon::Audio),
            "bluetooth" => list(addons.bluetooth.enabled, CommandListAddon::Bluetooth),
            "pass" => list(addons.pass.enabled, CommandListAddon::Pass),
            _ => {
                if let Some(source) = name.strip_prefix("text_snippets.") {
                    let source = source.trim_matches('"');
                    addons
                        .text_snippets
                        .iter()
                        .position(|config| config.name == source)
                        .map(Entry::Snippets)
                        .ok_or_else(|| {
                            format!("main_view text snippet source '{source}' not found in addons")
                        })
                } else if let Some(filter) = name.strip_prefix("script_filters.") {
                    let filter = filter.trim_matches('"');
                    addons
                        .script_filters
                        .iter()
                        .position(|config| config.name == filter)
                        .map(Entry::ScriptFilter)
                        .ok_or_else(|| {
                            format!("main_view script filter '{filter}' not found in addons")
                        })
                } else {
                    Err(format!("unsupported main_view entry '{raw}'"))
                }
            }
        };
        match entry {
            Ok(entry) if resolved.contains(&entry) => {
                crate::debug_log!("main_view: duplicate entry '{raw}' skipped");
            }
            Ok(entry) => resolved.push(entry),
            Err(warning) => {
                crate::debug_log!("main_view: {warning}");
                eprintln!("Warning: {warning}");
            }
        }
    }
    resolved
}

/// The `main_view` name a command list addon's picks are kept under, for
/// the addons whose picks come back on an empty query.
pub(super) fn recent_list_name(addon: CommandListAddon) -> Option<&'static str> {
    match addon {
        CommandListAddon::Windows => Some("windows"),
        CommandListAddon::BrowserTabs => Some("browser_tabs"),
        CommandListAddon::Pass => Some("pass"),
        _ => None,
    }
}

/// Whether a loaded row may show in the main view: rows that only inform or
/// set things up, which match whatever the query, stay in the addon's own
/// list.
pub(super) fn is_mixable(addon: CommandListAddon, item: &CommandItem) -> bool {
    if item.match_text.is_empty() || item.unavailable.is_some() {
        return false;
    }
    match addon {
        CommandListAddon::BrowserTabs => super::browser_tabs::tab_target(item).is_some(),
        CommandListAddon::Pass => super::pass::entry_name(&item.identity).is_some(),
        _ => item.primary.is_some(),
    }
}

pub(super) enum Source {
    /// Command list addon rows, ranked like the addon's own list.
    List(Box<CommandListState>),
    Snippets {
        config_index: usize,
        loading: bool,
        items: Vec<TextSnippet>,
        filtered: Vec<usize>,
        /// Values with their placeholders filled in, indexed like `items`.
        previews: Vec<Option<String>>,
        clipboard: ClipboardSnapshot,
    },
    ScriptFilter {
        config_index: usize,
        /// Bumped by every query, so late runs are dropped.
        generation: u64,
        /// Copy of `generation` read by debouncing threads.
        live: Arc<AtomicU64>,
        /// Query of the latest run.
        searched: String,
        /// Whether a run is out; the next one waits for it, so a slow
        /// script never runs more than once at a time.
        running: bool,
        results: Option<ScriptFilterResult>,
    },
}

/// Where a group of main view rows sits relative to the launchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Block {
    BeforeLaunchers,
    AfterLaunchers,
}

/// What Ctrl+Tab narrows the main view to; None in `MainViewState::focus`
/// is the mixed list of everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Launchers,
    Source(usize),
}

#[derive(Default)]
pub(super) struct MainViewState {
    /// Set when `main_view` is configured without `launchers`.
    pub hide_launchers: bool,
    /// Sources listed before `launchers`; all of them when it is not listed.
    pub launchers_at: usize,
    pub sources: Vec<Source>,
    /// Trimmed standard query the sources are filtered with.
    pub query: String,
    /// Whether the standard mode shows, the only one with main view rows.
    pub active: bool,
    /// What Ctrl+Tab cycles through in config order, before coming back to
    /// everything; empty when `main_view` is not configured.
    pub stops: Vec<Focus>,
    pub focus: Option<Focus>,
    /// Command list sources acted on in their addon's mode, reloaded when
    /// they show again.
    pub stale: HashSet<usize>,
    /// Name each source's picks are kept under, None for sources whose
    /// picks are not kept.
    pub names: Vec<Option<String>>,
    /// Rows picked from the sources, newest first, listed again when the
    /// query is empty.
    pub recent: Vec<RecentPick>,
    pub recent_path: Option<PathBuf>,
    /// Compositor instance window picks must come from.
    pub window_session: Option<String>,
    /// Whether a launcher matching the query starts with it or has a word
    /// that does, which lists the launchers ahead of weaker groups.
    pub launchers_strong: bool,
    /// Whether the sources' rows are the recent picks rather than matches.
    recent_applied: bool,
}

impl MainViewState {
    pub fn new(entries: &[Entry]) -> Self {
        let mut state = Self {
            hide_launchers: !entries.contains(&Entry::Launchers),
            active: true,
            ..Default::default()
        };
        for entry in entries {
            let source = match *entry {
                Entry::Launchers => {
                    state.launchers_at = state.sources.len();
                    state.stops.push(Focus::Launchers);
                    continue;
                }
                Entry::List(addon) => {
                    let mut list = CommandListState::default();
                    list.enter(addon);
                    list.session = MAIN_VIEW_SESSION | state.sources.len() as u64;
                    list.loading = true;
                    list.strict = true;
                    Source::List(Box::new(list))
                }
                Entry::Snippets(config_index) => Source::Snippets {
                    config_index,
                    loading: false,
                    items: Vec::new(),
                    filtered: Vec::new(),
                    previews: Vec::new(),
                    clipboard: ClipboardSnapshot::default(),
                },
                Entry::ScriptFilter(config_index) => Source::ScriptFilter {
                    config_index,
                    generation: 0,
                    live: Arc::default(),
                    searched: String::new(),
                    running: false,
                    results: None,
                },
            };
            state.stops.push(Focus::Source(state.sources.len()));
            state.sources.push(source);
        }
        if state.hide_launchers {
            state.launchers_at = state.sources.len();
        }
        state
    }

    /// Move the focus to the next stop, or the previous one, everything
    /// coming after the last stop.
    pub fn cycle(&mut self, backwards: bool) {
        let all = self.stops.len();
        let at = self
            .focus
            .and_then(|focus| self.stops.iter().position(|&stop| stop == focus))
            .unwrap_or(all);
        let next = if backwards {
            (at + all) % (all + 1)
        } else {
            (at + 1) % (all + 1)
        };
        self.focus = self.stops.get(next).copied();
    }

    /// The focus while the standard mode shows.
    pub fn focused(&self) -> Option<Focus> {
        self.focus.filter(|_| self.active)
    }

    /// Whether `source` lists rows now: on its own whatever the query,
    /// mixed with the others once there is one, or its recent picks when
    /// there is none.
    pub fn shows(&self, source: usize) -> bool {
        self.active
            && match self.focus {
                None => !self.query.is_empty() || self.has_picks(source),
                Some(focus) => focus == Focus::Source(source),
            }
    }

    pub fn has_loading(&self) -> bool {
        self.sources.iter().enumerate().any(|(source, entry)| {
            self.shows(source)
                && match entry {
                    Source::List(list) => list.loading || list.refreshing,
                    Source::Snippets { loading, .. } => *loading,
                    Source::ScriptFilter { running, .. } => *running,
                }
        })
    }

    /// A list source whose load failed or never starts, so its recent
    /// picks stop waiting for rows.
    pub fn loaded_nothing(&mut self, source: usize) {
        if let Some(Source::List(list)) = self.sources.get_mut(source) {
            list.loading = false;
            list.refreshing = false;
        }
        self.apply_recent();
    }

    fn has_picks(&self, source: usize) -> bool {
        self.names
            .get(source)
            .and_then(Option::as_deref)
            .is_some_and(|name| self.recent.iter().any(|pick| pick.source == name))
    }

    fn recent_mode(&self) -> bool {
        self.active && self.focus.is_none() && self.query.is_empty()
    }

    /// With an empty query and no focus, list the newest picks that can
    /// still be found as the rows of their sources, at most `MAX_ROWS` in
    /// all; otherwise give the sources back their matches. Call after the
    /// sources were filtered.
    pub fn apply_recent(&mut self) {
        if !self.recent_mode() {
            if std::mem::take(&mut self.recent_applied) {
                for source in &mut self.sources {
                    match source {
                        Source::List(list) => {
                            let query = list.query.clone();
                            list.set_query(query);
                        }
                        Source::ScriptFilter { results, .. } => *results = None,
                        // Filtered again by every refresh.
                        Source::Snippets { .. } => {}
                    }
                }
            }
            return;
        }
        self.recent_applied = true;
        let mut rows = vec![Vec::new(); self.sources.len()];
        let mut stored = vec![Vec::new(); self.sources.len()];
        let mut taken = 0;
        for pick in &self.recent {
            if taken == MAX_ROWS {
                break;
            }
            let Some(source) = self
                .names
                .iter()
                .position(|name| name.as_deref() == Some(pick.source.as_str()))
            else {
                continue;
            };
            let picked: &[usize] = &rows[source];
            let found = match &self.sources[source] {
                Source::List(list)
                    if list.addon == Some(CommandListAddon::Windows)
                        && pick.session != self.window_session =>
                {
                    continue;
                }
                // Keeps its place while the rows load, so they do not push
                // out rows already shown.
                Source::List(list) if list.loading => {
                    taken += 1;
                    continue;
                }
                Source::List(list) => find_pick(&list.items, pick, picked),
                Source::Snippets { items, .. } => items
                    .iter()
                    .position(|snippet| snippet.name == pick.key)
                    .filter(|index| !picked.contains(index)),
                Source::ScriptFilter { .. } => {
                    if let Some(item) = &pick.item {
                        stored[source].push(item.to_item());
                        taken += 1;
                    }
                    continue;
                }
            };
            if let Some(index) = found {
                rows[source].push(index);
                taken += 1;
            }
        }
        for (source, (rows, stored)) in rows.into_iter().zip(stored).enumerate() {
            if !self.has_picks(source) {
                continue;
            }
            match &mut self.sources[source] {
                Source::List(list) => list.filtered = rows,
                Source::Snippets { filtered, .. } => *filtered = rows,
                Source::ScriptFilter { results, .. } => {
                    *results = (!stored.is_empty()).then_some(ScriptFilterResult {
                        items: stored,
                        default_icon: None,
                    });
                }
            }
        }
    }

    /// Sources in display order and how many of them come before the
    /// launchers. Config order, except that while a query is typed with
    /// nothing focused, the groups with a row whose title starts with the
    /// query, or has a word that does, move ahead of the others.
    fn arrangement(&self) -> (Vec<usize>, usize) {
        if !self.active || self.focus.is_some() || self.query.is_empty() {
            return ((0..self.sources.len()).collect(), self.launchers_at);
        }
        let groups = (0..self.launchers_at)
            .map(Some)
            .chain(std::iter::once(None))
            .chain((self.launchers_at..self.sources.len()).map(Some));
        let (strong, weak): (Vec<_>, Vec<_>) = groups.partition(|group| match group {
            Some(source) => self.is_strong(*source),
            None => self.launchers_strong && !self.hide_launchers,
        });
        let groups: Vec<Option<usize>> = strong.into_iter().chain(weak).collect();
        let split = groups.iter().position(Option::is_none).unwrap_or_default();
        (groups.into_iter().flatten().collect(), split)
    }

    /// Whether one of the rows `source` shows has a title starting with the
    /// query or with a word that does.
    fn is_strong(&self, source: usize) -> bool {
        let rows = self.source_rows(source);
        let strong = |title: &str| super::support::word_prefix_match(title, &self.query);
        match &self.sources[source] {
            Source::List(list) => (0..rows)
                .filter_map(|row| list.item(row))
                .any(|item| strong(&item.title)),
            Source::Snippets {
                items, filtered, ..
            } => filtered
                .iter()
                .take(rows)
                .any(|&index| strong(&items[index].name)),
            Source::ScriptFilter { results, .. } => results
                .iter()
                .flat_map(|result| result.items.iter().take(rows))
                .any(|item| strong(&item.title)),
        }
    }

    fn block_sources(&self, block: Block) -> Vec<usize> {
        let (mut order, split) = self.arrangement();
        match block {
            Block::BeforeLaunchers => {
                order.truncate(split);
                order
            }
            Block::AfterLaunchers => order.split_off(split),
        }
    }

    /// Rows `source` shows for the current query, all of its matches when
    /// it has the focus.
    pub fn source_rows(&self, source: usize) -> usize {
        if !self.shows(source) {
            return 0;
        }
        let count = match &self.sources[source] {
            Source::List(list) if list.status().is_some() => 0,
            Source::List(list) => list.filtered.len(),
            Source::Snippets { filtered, .. } => filtered.len(),
            Source::ScriptFilter { results, .. } => {
                results.as_ref().map_or(0, |result| result.items.len())
            }
        };
        if self.focus.is_some() {
            count
        } else {
            count.min(MAX_ROWS)
        }
    }

    pub fn row_count(&self, block: Block) -> usize {
        self.block_sources(block)
            .into_iter()
            .map(|source| self.source_rows(source))
            .sum()
    }

    /// The rows of `block` in display order, as (source, row in source).
    pub fn rows(&self, block: Block) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.block_sources(block)
            .into_iter()
            .flat_map(move |source| (0..self.source_rows(source)).map(move |row| (source, row)))
    }

    /// The source and row in it shown at `row` of `block`.
    pub fn locate(&self, block: Block, row: usize) -> Option<(usize, usize)> {
        self.rows(block).nth(row)
    }

    pub fn block_of(&self, source: usize) -> Block {
        if self.block_sources(Block::BeforeLaunchers).contains(&source) {
            Block::BeforeLaunchers
        } else {
            Block::AfterLaunchers
        }
    }

    /// Rows of the sources shown before `source` in its block.
    fn rows_before(&self, source: usize) -> usize {
        self.block_sources(self.block_of(source))
            .into_iter()
            .take_while(|&other| other != source)
            .map(|other| self.source_rows(other))
            .sum()
    }
}

/// The item `pick` was made on and not yet listed: the same identity, or
/// for a tab the same page, its id changing when the browser restarts.
fn find_pick(items: &[CommandItem], pick: &RecentPick, picked: &[usize]) -> Option<usize> {
    let free = |index: &usize| !picked.contains(index);
    items
        .iter()
        .position(|item| item.identity == pick.key)
        .filter(free)
        .or_else(|| {
            let url = pick.url.as_deref()?;
            items.iter().enumerate().position(|(index, item)| {
                free(&index)
                    && super::browser_tabs::tab_target(item).is_some_and(|tab| tab.url == url)
            })
        })
}

/// Filter a snippet source with the main view query, ranked by `usage`.
/// Past the keyword matches, the name or value must hold every word of the
/// query, a loose fuzzy match being noise next to the other sources.
fn filter_main_view_snippets(main_view: &mut MainViewState, source: usize, usage: &[u64]) {
    let shows = main_view.shows(source);
    let query = &main_view.query;
    if let Some(Source::Snippets {
        items, filtered, ..
    }) = main_view.sources.get_mut(source)
    {
        *filtered = if shows {
            let lowered = query.to_lowercase();
            let mut matches = filter_snippets(items, query, usage);
            matches.retain(|&index| {
                let snippet = &items[index];
                query.is_empty()
                    || snippet
                        .keyword
                        .as_deref()
                        .is_some_and(|keyword| keyword.to_lowercase().starts_with(&lowered))
                    || contains_words(&snippet.name, query)
                    || contains_words(&snippet.value, query)
            });
            matches
        } else {
            Vec::new()
        };
    }
}

/// Run `work` on its own thread; None if it panicked.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.ok()
}

type ListLoad = Box<dyn FnOnce() -> Result<Vec<CommandItem>, String> + Send>;

impl LauncherApp {
    /// Set up `general.main_view` and start loading its addons.
    pub(super) fn configure_main_view(&mut self, entries: Option<&[String]>) -> Task<Message> {
        let Some(entries) = entries else {
            return Task::none();
        };
        let entries = resolve(entries, &self.addons);
        self.main_view = MainViewState::new(&entries);
        self.main_view.names = entries
            .iter()
            .filter_map(|entry| match *entry {
                Entry::Launchers => None,
                Entry::List(addon) => Some(recent_list_name(addon).map(str::to_string)),
                Entry::Snippets(index) => Some(
                    self.addons
                        .text_snippets
                        .get(index)
                        .map(|config| format!("text_snippets.{}", config.name)),
                ),
                Entry::ScriptFilter(index) => Some(
                    self.addons
                        .script_filters
                        .get(index)
                        .map(|config| format!("script_filters.{}", config.name)),
                ),
            })
            .collect();
        self.main_view.recent_path = crate::ui::get_recent_cache_path().ok();
        self.main_view.window_session =
            super::windows::Compositor::instance(|name| std::env::var(name).ok());
        self.main_view.recent = super::recent::load(self.main_view.recent_path.as_deref());
        if self.main_view.hide_launchers {
            self.filtered_configs.clear();
        }
        self.main_view.apply_recent();
        let loads: Vec<_> = (0..self.main_view.sources.len())
            .map(|source| self.load_main_view_source(source, true))
            .collect();
        Task::batch(loads)
    }

    /// Keep `pick` as the newest recent pick, when its source is listed in
    /// `main_view`.
    pub(super) fn record_recent(&mut self, pick: RecentPick) {
        let listed = self
            .main_view
            .names
            .iter()
            .any(|name| name.as_deref() == Some(pick.source.as_str()));
        if !listed {
            return;
        }
        if let Some(path) = &self.main_view.recent_path {
            // Picks another Raffi made since this one started are kept.
            self.main_view.recent = super::recent::load(Some(path));
        }
        super::recent::record(&mut self.main_view.recent, pick);
        super::recent::save(
            self.main_view.recent_path.as_deref(),
            &self.main_view.recent,
        );
    }

    fn load_main_view_source(&mut self, source: usize, fetch_favicons: bool) -> Task<Message> {
        match &self.main_view.sources[source] {
            Source::List(list) => match list.addon {
                Some(addon) => self.load_main_view_list(source, addon, fetch_favicons),
                None => Task::none(),
            },
            Source::Snippets { config_index, .. } => {
                let config_index = *config_index;
                let Some(config) = self.addons.text_snippets.get(config_index).cloned() else {
                    if let Some(Source::Snippets { loading, .. }) =
                        self.main_view.sources.get_mut(source)
                    {
                        *loading = false;
                    }
                    return Task::none();
                };
                if let Some(Source::Snippets { loading, .. }) =
                    self.main_view.sources.get_mut(source)
                {
                    *loading = true;
                }
                Task::perform(
                    off_thread(move || load_snippet_source(&config)),
                    move |result| {
                        Message::MainViewSnippetsLoaded(
                            source,
                            result.unwrap_or_else(|| Err("loading stopped unexpectedly".into())),
                        )
                    },
                )
            }
            Source::ScriptFilter { .. } => Task::none(),
        }
    }

    /// Load the rows of a command list source, leaving it hidden when the
    /// addon cannot run here.
    fn load_main_view_list(
        &mut self,
        source: usize,
        addon: CommandListAddon,
        fetch_favicons: bool,
    ) -> Task<Message> {
        let generation = {
            let Source::List(list) = &mut self.main_view.sources[source] else {
                return Task::none();
            };
            list.generation = list.generation.wrapping_add(1);
            if list.items.is_empty() {
                list.loading = true;
            } else {
                list.refreshing = true;
            }
            list.generation
        };
        let exists = self.program_exists;
        let missing = |this: &mut Self, tool: &str| {
            crate::debug_log!("main_view: {} hidden, {tool} not found", addon.name());
            this.update_main_view(|main_view| main_view.loaded_nothing(source));
            Task::none()
        };
        let load: ListLoad = match addon {
            CommandListAddon::System => {
                let session_id = std::env::var("XDG_SESSION_ID").ok();
                let items = super::system::system_entries(
                    &self.addons.system,
                    (self.compositor)(),
                    session_id.as_deref().filter(|id| !id.is_empty()),
                    self.program_exists,
                )
                .iter()
                .map(|entry| entry.to_item())
                .collect();
                return self.handle_main_view_loaded(source, generation, Ok(items));
            }
            CommandListAddon::Windows => {
                let Some(compositor) = (self.compositor)() else {
                    return missing(self, "a supported compositor");
                };
                if !exists(compositor.program()) {
                    return missing(self, compositor.program());
                }
                Box::new(move || compositor.load_windows())
            }
            CommandListAddon::BrowserTabs => Box::new(super::browser_tabs::load_tabs),
            CommandListAddon::Media => {
                if !exists("playerctl") {
                    return missing(self, "playerctl");
                }
                let keep_open = !self.addons.media.close_on_action;
                Box::new(move || super::media::load_players(keep_open))
            }
            CommandListAddon::Audio => {
                if !exists("pactl") {
                    return missing(self, "pactl");
                }
                let options = super::audio::AudioOptions::of(&self.addons.audio);
                let query_row = self.audio_query_row(false);
                if let Source::List(list) = &mut self.main_view.sources[source] {
                    list.query_row = Some(query_row);
                }
                Box::new(move || Ok(super::audio::load_devices(options)))
            }
            CommandListAddon::Bluetooth => {
                if !exists("bluetoothctl") {
                    return missing(self, "bluetoothctl");
                }
                let keep_open = !self.addons.bluetooth.close_on_action;
                Box::new(move || Ok(super::bluetooth::load_devices(keep_open)))
            }
            CommandListAddon::Pass => {
                if !exists("pass") {
                    return missing(self, "pass");
                }
                if !exists("gpg") && !exists("gpg2") {
                    return missing(self, "gpg");
                }
                let usage = self.vault_usage();
                if let Source::List(list) = &mut self.main_view.sources[source] {
                    list.usage = usage;
                }
                let ctx = super::pass::PassContext::new(&self.addons.pass);
                Box::new(move || super::pass::load_entries(&ctx))
            }
            CommandListAddon::Kill
            | CommandListAddon::Bitwarden
            | CommandListAddon::Terminal
            | CommandListAddon::Slack => {
                self.update_main_view(|main_view| main_view.loaded_nothing(source));
                return Task::none();
            }
        };
        let load = Task::perform(off_thread(load), move |result| {
            Message::MainViewLoaded(
                source,
                generation,
                result.unwrap_or_else(|| Err("loading stopped unexpectedly".into())),
            )
        });
        if addon != CommandListAddon::BrowserTabs || !fetch_favicons {
            return load;
        }
        let session = MAIN_VIEW_SESSION | source as u64;
        let service = self.addons.browser_tabs.favicon_service;
        load.chain(Task::perform(
            off_thread(move || super::favicons::fetch_pending(service)),
            move |fetched| Message::FaviconsFetched(session, fetched.unwrap_or(false)),
        ))
    }

    pub(super) fn handle_main_view_loaded(
        &mut self,
        source: usize,
        generation: u64,
        result: Result<Vec<CommandItem>, String>,
    ) -> Task<Message> {
        let Some(Source::List(list)) = self.main_view.sources.get(source) else {
            return Task::none();
        };
        if list.generation != generation {
            // A later load is out, which sees more recent state.
            return Task::none();
        }
        let Some(addon) = list.addon else {
            return Task::none();
        };
        let mut items = match result {
            Ok(items) => items,
            Err(error) => {
                crate::debug_log!("main_view: {} hidden: {error}", addon.name());
                self.update_main_view(|main_view| main_view.loaded_nothing(source));
                return Task::none();
            }
        };
        items.retain(|item| is_mixable(addon, item));
        for item in &mut items {
            item.icon = item
                .icon_names
                .iter()
                .find_map(|name| self.resolve_icon_name(name));
        }
        let query = self.main_view.query.clone();
        self.update_main_view(|main_view| {
            if let Some(Source::List(list)) = main_view.sources.get_mut(source) {
                list.set_items(items);
                list.set_query(query);
            }
            main_view.apply_recent();
        });
        Task::none()
    }

    /// Change the main view with `update`, keeping the selection on the row
    /// it was on, so rows landing above it never change what Enter runs.
    /// Recent picks share their rows across sources, so one source loading
    /// can move the rows of the others.
    fn update_main_view(&mut self, update: impl FnOnce(&mut MainViewState)) {
        if self.view.help_active {
            update(&mut self.main_view);
            return;
        }
        let before_total = self.total_items();
        let selected = self.selected_index;
        let launchers = self.launchers_start();
        let in_launchers = (launchers..launchers + self.filtered_configs.len()).contains(&selected);
        let before = self.main_view_spans();
        let within = before
            .iter()
            .position(|&(start, count)| (start..start + count).contains(&selected))
            .map(|source| {
                let row = selected - before[source].0;
                let identity = match &self.main_view.sources[source] {
                    Source::List(list) => list.item(row).map(|item| item.identity.clone()),
                    _ => None,
                };
                (source, row, identity)
            });
        update(&mut self.main_view);
        if selected >= before_total {
            return;
        }
        let after = self.main_view_spans();
        self.selected_index = match within {
            Some((source, row, identity)) => {
                let (start, count) = after[source];
                let found = identity
                    .and_then(|identity| match &self.main_view.sources[source] {
                        Source::List(list) => list.position(&identity),
                        _ => None,
                    })
                    .filter(|&row| row < count);
                // When the selected row went away, select what now sits there.
                start + found.unwrap_or(row.min(count.saturating_sub(1)))
            }
            None if in_launchers => self.launchers_start() + selected - launchers,
            None => {
                let moved: isize = before
                    .iter()
                    .zip(&after)
                    .filter(|((start, count), _)| start + count <= selected)
                    .map(|((_, old), (_, new))| *new as isize - *old as isize)
                    .sum();
                selected.saturating_add_signed(moved)
            }
        };
        self.selected_index = self
            .selected_index
            .min(self.total_items().saturating_sub(1));
    }

    /// Index of the first launcher row in the whole list.
    fn launchers_start(&self) -> usize {
        self.main_view_offset(Block::AfterLaunchers)
            .saturating_sub(self.filtered_configs.len())
    }

    /// First row and row count of every source in the whole list.
    fn main_view_spans(&self) -> Vec<(usize, usize)> {
        (0..self.main_view.sources.len())
            .map(|source| {
                let start = self.main_view_offset(self.main_view.block_of(source))
                    + self.main_view.rows_before(source);
                (start, self.main_view.source_rows(source))
            })
            .collect()
    }

    /// Reload the main view sources of `addon` after one of its actions
    /// ran, wherever it was started: now when they show, else the next
    /// time they do.
    pub(super) fn invalidate_main_view(&mut self, addon: CommandListAddon) -> Task<Message> {
        let sources: Vec<usize> = (0..self.main_view.sources.len())
            .filter(|&source| {
                matches!(&self.main_view.sources[source], Source::List(list) if list.addon == Some(addon))
            })
            .collect();
        let mut tasks = Vec::new();
        for source in sources {
            if self.main_view.shows(source) {
                self.main_view.stale.remove(&source);
                tasks.push(self.load_main_view_source(source, false));
            } else {
                self.main_view.stale.insert(source);
            }
        }
        Task::batch(tasks)
    }

    pub(super) fn handle_main_view_favicons(
        &mut self,
        session: u64,
        fetched: bool,
    ) -> Task<Message> {
        let source = (session & !MAIN_VIEW_SESSION) as usize;
        if fetched && matches!(self.main_view.sources.get(source), Some(Source::List(_))) {
            self.load_main_view_source(source, false)
        } else {
            Task::none()
        }
    }

    pub(super) fn handle_main_view_snippets_loaded(
        &mut self,
        source: usize,
        result: Result<Vec<TextSnippet>, String>,
    ) -> Task<Message> {
        if let Some(Source::Snippets { loading, .. }) = self.main_view.sources.get_mut(source) {
            *loading = false;
        }
        match result {
            Ok(snippets) => {
                if let Some(Source::Snippets { items, .. }) = self.main_view.sources.get_mut(source)
                {
                    *items = snippets;
                }
                let usage = self.main_view_snippet_usage(source);
                self.update_main_view(|main_view| {
                    filter_main_view_snippets(main_view, source, &usage);
                    main_view.apply_recent();
                });
                self.compute_main_view_snippet_previews(source);
                return self.fetch_main_view_clipboard(source);
            }
            Err(error) => crate::debug_log!("main_view: snippet source hidden: {error}"),
        }
        Task::none()
    }

    /// Fill in the placeholders of a snippet source's values for its rows,
    /// as its own mode does.
    fn compute_main_view_snippet_previews(&mut self, source: usize) {
        let Some(Source::Snippets {
            config_index,
            items,
            previews,
            clipboard,
            ..
        }) = self.main_view.sources.get_mut(source)
        else {
            return;
        };
        let Some(config) = self.addons.text_snippets.get(*config_index) else {
            return;
        };
        *previews = if config.placeholders {
            // Command output is not written by the user, so it may not run shell code.
            let allow_shell = config.command.is_none();
            compute_previews(items, allow_shell, &clipboard.data)
        } else {
            Vec::new()
        };
    }

    /// Read the clipboard entries a snippet source's previews need, once its
    /// rows show.
    fn fetch_main_view_clipboard(&mut self, source: usize) -> Task<Message> {
        let history = self.text_snippets.clipboard_history;
        let hidden = !self.main_view.shows(source);
        let Some(Source::Snippets {
            config_index,
            items,
            clipboard,
            ..
        }) = self.main_view.sources.get_mut(source)
        else {
            return Task::none();
        };
        let placeholders = self
            .addons
            .text_snippets
            .get(*config_index)
            .is_some_and(|config| config.placeholders);
        if hidden || !placeholders || clipboard.fetching {
            return Task::none();
        }
        let needs = missing_clipboard(items, &clipboard.data);
        if needs.is_empty() {
            return Task::none();
        }
        clipboard.fetching = true;
        fetch_clipboard(MAIN_VIEW_SESSION | source as u64, needs, history)
    }

    pub(super) fn handle_main_view_clipboard(
        &mut self,
        session: u64,
        data: ClipboardData,
    ) -> Task<Message> {
        let source = (session & !MAIN_VIEW_SESSION) as usize;
        if let Some(Source::Snippets { clipboard, .. }) = self.main_view.sources.get_mut(source) {
            clipboard.fetching = false;
            clipboard.data.merge(data);
            self.compute_main_view_snippet_previews(source);
        }
        Task::none()
    }

    /// Usage ranking of a snippet source's items.
    fn main_view_snippet_usage(&self, source: usize) -> Vec<u64> {
        let Some(Source::Snippets {
            config_index,
            items,
            ..
        }) = self.main_view.sources.get(source)
        else {
            return Vec::new();
        };
        let keyword = self
            .addons
            .text_snippets
            .get(*config_index)
            .map(|config| config.keyword.as_str())
            .unwrap_or_default();
        let map = &self.text_snippets.mru_map;
        let (max_count, min_ts, max_ts) = mru_bounds(map);
        items
            .iter()
            .map(|snippet| {
                let key = snippet_mru_key(keyword, &snippet.name);
                mru_sort_key(&key, map, &self.sort_mode, max_count, min_ts, max_ts)
            })
            .collect()
    }

    /// Filter every source with `query`, the trimmed standard mode query.
    /// Sources load and script filters run only while they show.
    pub(super) fn refresh_main_view(&mut self, query: &str) -> Task<Message> {
        self.main_view.query = query.to_string();
        let mut tasks = Vec::new();
        for source in 0..self.main_view.sources.len() {
            let shows = self.main_view.shows(source);
            match &mut self.main_view.sources[source] {
                Source::List(list) => {
                    if list.query != query {
                        list.set_query(query.to_string());
                    }
                    if shows && self.main_view.stale.remove(&source) {
                        tasks.push(self.load_main_view_source(source, false));
                    }
                }
                Source::Snippets { .. } => {
                    let usage = self.main_view_snippet_usage(source);
                    filter_main_view_snippets(&mut self.main_view, source, &usage);
                    tasks.push(self.fetch_main_view_clipboard(source));
                }
                Source::ScriptFilter { .. } => {
                    // A hidden one forgets its results, never shown for a stale query.
                    let query = if shows { query } else { "" };
                    tasks.push(self.run_main_view_script_filter(source, query.to_string()));
                }
            }
        }
        self.main_view.apply_recent();
        Task::batch(tasks)
    }

    /// Run a script filter source for `query` once typing pauses. Earlier
    /// results stay listed until the new ones arrive.
    fn run_main_view_script_filter(&mut self, source: usize, query: String) -> Task<Message> {
        let Some(Source::ScriptFilter {
            generation,
            live,
            searched,
            results,
            ..
        }) = self.main_view.sources.get_mut(source)
        else {
            return Task::none();
        };
        if *searched == query {
            return Task::none();
        }
        searched.clone_from(&query);
        *generation = generation.wrapping_add(1);
        live.store(*generation, Ordering::Relaxed);
        if query.is_empty() {
            *results = None;
        }
        self.start_main_view_script_filter(source)
    }

    /// Start a run for the latest query, unless one is still out: that one
    /// starts the next when it ends.
    fn start_main_view_script_filter(&mut self, source: usize) -> Task<Message> {
        let Some(Source::ScriptFilter {
            config_index,
            generation,
            live,
            searched,
            running,
            results,
        }) = self.main_view.sources.get_mut(source)
        else {
            return Task::none();
        };
        let Some(config) = self.addons.script_filters.get(*config_index) else {
            return Task::none();
        };
        if searched.is_empty() {
            // Cleared when the query emptied; now it may hold recent picks.
            return Task::none();
        }
        if searched.len() < config.min_query_length.unwrap_or(0) {
            *results = None;
            return Task::none();
        }
        if *running {
            return Task::none();
        }
        *running = true;
        let generation = *generation;
        let live = live.clone();
        let query = searched.clone();
        let mut command = Command::new(&config.command);
        command.args(&config.args).envs(&config.env).arg(&query);
        let icon = config.icon.clone();
        Task::perform(
            off_thread(move || {
                std::thread::sleep(SCRIPT_FILTER_DEBOUNCE);
                (live.load(Ordering::Relaxed) == generation).then(|| {
                    crate::debug_log!("main_view: script filter query={query:?}");
                    super::actions::run_bounded(
                        command,
                        SCRIPT_FILTER_TIMEOUT,
                        SCRIPT_FILTER_MAX_OUTPUT,
                    )
                    .and_then(|output| {
                        super::script_filters::parse_script_filter_output(
                            &String::from_utf8_lossy(&output),
                            icon,
                        )
                    })
                })
            }),
            move |result| Message::MainViewScriptFilter(source, generation, result.flatten()),
        )
    }

    pub(super) fn handle_main_view_script_filter(
        &mut self,
        source: usize,
        run: u64,
        result: Option<Result<ScriptFilterResult, String>>,
    ) -> Task<Message> {
        let Some(Source::ScriptFilter {
            generation,
            running,
            ..
        }) = self.main_view.sources.get_mut(source)
        else {
            return Task::none();
        };
        *running = false;
        if *generation != run {
            return self.start_main_view_script_filter(source);
        }
        let result = match result {
            Some(Ok(result)) => Some(result),
            Some(Err(error)) => {
                crate::debug_log!("main_view: script filter hidden: {error}");
                None
            }
            None => return Task::none(),
        };
        self.update_main_view(|main_view| {
            if let Some(Source::ScriptFilter { results, .. }) = main_view.sources.get_mut(source) {
                *results = result;
            }
        });
        Task::none()
    }

    pub(super) fn handle_main_view_tab_preview(
        &mut self,
        session: u64,
        url: String,
        bodies: Option<Vec<super::preview::PreviewBody>>,
    ) -> Task<Message> {
        let source = (session & !MAIN_VIEW_SESSION) as usize;
        if let Some(Source::List(list)) = self.main_view.sources.get_mut(source) {
            if list.preview_inflight.as_ref() == Some(&url) {
                list.preview_inflight = None;
            }
            if let Some(bodies) = bodies {
                list.previews.insert(url, bodies);
            }
        }
        Task::none()
    }

    /// Name of the current Ctrl+Tab stop, shown in the footer.
    pub(super) fn main_view_focus_label(&self) -> String {
        let Some(Focus::Source(source)) = self.main_view.focus else {
            let label = if self.main_view.focus.is_some() {
                "Launchers"
            } else {
                "All"
            };
            return label.to_string();
        };
        match &self.main_view.sources[source] {
            Source::List(list) => match list.addon {
                Some(CommandListAddon::BrowserTabs) => "Tabs",
                Some(CommandListAddon::Windows) => "Windows",
                Some(CommandListAddon::Pass) => "Passwords",
                Some(addon) => addon.badge(),
                None => "",
            }
            .to_string(),
            Source::Snippets { config_index, .. } => self
                .addons
                .text_snippets
                .get(*config_index)
                .map(|config| config.name.clone())
                .unwrap_or_default(),
            Source::ScriptFilter { config_index, .. } => self
                .addons
                .script_filters
                .get(*config_index)
                .map(|config| config.name.clone())
                .unwrap_or_default(),
        }
    }

    /// Index of the first main view row of `block`.
    pub(super) fn main_view_offset(&self, block: Block) -> usize {
        let after = self.main_view.row_count(Block::AfterLaunchers);
        let end = self
            .total_items()
            .saturating_sub(self.fallback_count() + after);
        match block {
            Block::AfterLaunchers => end,
            Block::BeforeLaunchers => end.saturating_sub(
                self.filtered_configs.len() + self.main_view.row_count(Block::BeforeLaunchers),
            ),
        }
    }

    /// The main view source and row in it that is selected, if any.
    pub(super) fn selected_main_view(&self) -> Option<(usize, usize)> {
        if self.view.help_active {
            return None;
        }
        [Block::BeforeLaunchers, Block::AfterLaunchers]
            .into_iter()
            .find_map(|block| {
                let row = self
                    .selected_index
                    .checked_sub(self.main_view_offset(block))?;
                (row < self.main_view.row_count(block))
                    .then(|| self.main_view.locate(block, row))
                    .flatten()
            })
    }

    /// Fetch the page preview of the selected main view tab, forgetting the
    /// wish of every other tab list.
    pub(super) fn ensure_main_view_tab_preview(
        &mut self,
        service: crate::FaviconService,
    ) -> Task<Message> {
        let selected = self
            .view
            .show_preview
            .then(|| self.selected_main_view())
            .flatten();
        let mut task = Task::none();
        for (index, source) in self.main_view.sources.iter_mut().enumerate() {
            let Source::List(list) = source else {
                continue;
            };
            let tab = match selected {
                Some((source, row)) if source == index => list
                    .item(row)
                    .and_then(super::browser_tabs::tab_target)
                    .cloned(),
                _ => None,
            };
            match tab {
                Some(tab) => task = super::app::request_tab_preview(list, &tab, service),
                None => list.preview_wanted.clear(),
            }
        }
        task
    }

    /// Run a main view row by handing it to its addon's own mode, as if
    /// its trigger had been typed before the query, so it behaves exactly
    /// like there: confirmations, lists kept open, usage and history.
    pub(super) fn activate_main_view(&mut self, block: Block, row: usize) -> Task<Message> {
        let Some((source, row)) = self.main_view.locate(block, row) else {
            return Task::none();
        };
        if self.view.current_modifiers.alt() {
            return Task::none();
        }
        let query = self.main_view.query.clone();
        match &self.main_view.sources[source] {
            Source::List(list) => {
                let Some(addon) = list.addon else {
                    return Task::none();
                };
                let Some(item) = list.item(row) else {
                    return Task::none();
                };
                let identity = item.identity.clone();
                let items = list.items.clone();
                let usage = list.usage.clone();
                let previews = list.previews.clone();
                let trigger = match addon {
                    CommandListAddon::System => self.addons.system.trigger(),
                    CommandListAddon::Windows => self.addons.windows.trigger(),
                    CommandListAddon::BrowserTabs => self.addons.browser_tabs.trigger(),
                    CommandListAddon::Media => self.addons.media.trigger(),
                    CommandListAddon::Audio => self.addons.audio.trigger(),
                    CommandListAddon::Bluetooth => self.addons.bluetooth.trigger(),
                    CommandListAddon::Pass => self.addons.pass.trigger(),
                    CommandListAddon::Kill
                    | CommandListAddon::Bitwarden
                    | CommandListAddon::Terminal
                    | CommandListAddon::Slack => return Task::none(),
                }
                .to_string();
                self.main_view.stale.insert(source);
                self.command_list.enter(addon);
                if addon == CommandListAddon::Audio {
                    // Replaces the main view's volume row in the copied items.
                    self.command_list.query_row = Some(self.audio_query_row(true));
                }
                self.command_list.usage = usage;
                self.command_list.previews = previews;
                self.command_list.set_items(items);
                let task = self.handle_search_changed(format!("{trigger} {query}"));
                if self.command_list.addon != Some(addon) {
                    return task;
                }
                let Some(position) = self.command_list.position(&identity) else {
                    return task;
                };
                self.selected_index = self.command_list_offset() + position;
                let cursor = move_cursor_to_end(self.view.search_input_id.clone());
                let selected = self.handle_command_list_selected(position);
                Task::batch([task, cursor, selected])
            }
            Source::Snippets {
                config_index,
                items,
                filtered,
                ..
            } => {
                let config_index = *config_index;
                let Some(&item) = filtered.get(row) else {
                    return Task::none();
                };
                let items = items.clone();
                let Some(keyword) = self
                    .addons
                    .text_snippets
                    .get(config_index)
                    .map(|config| config.keyword.clone())
                else {
                    return Task::none();
                };
                // Dropped: the snippets are already loaded, which also keeps
                // a command source from running again.
                let _ = self.handle_search_changed(format!("{keyword} {query}"));
                if !self.text_snippets.active
                    || self.text_snippets.source_index != Some(config_index)
                {
                    return Task::none();
                }
                self.text_snippets.generation = self.text_snippets.generation.wrapping_add(1);
                self.text_snippets.items = items;
                let mut tasks = Vec::new();
                self.finish_text_snippet_load(&query, &mut tasks);
                let Some(position) = self
                    .text_snippets
                    .filtered
                    .iter()
                    .position(|&index| index == item)
                else {
                    return Task::batch(tasks);
                };
                self.selected_index =
                    self.keyword_suggestions.len() + self.script_filter_offset() + position;
                tasks.push(self.handle_text_snippet_selected(position));
                Task::batch(tasks)
            }
            Source::ScriptFilter {
                config_index,
                results,
                ..
            } => {
                let config_index = *config_index;
                let Some(result) = results.clone() else {
                    return Task::none();
                };
                if !result.items.get(row).is_some_and(|item| item.valid) {
                    return Task::none();
                }
                let Some(keyword) = self
                    .addons
                    .script_filters
                    .get(config_index)
                    .map(|config| config.keyword.clone())
                else {
                    return Task::none();
                };
                // Dropped: the results are already known for this query.
                let _ = self.handle_search_changed(format!("{keyword} {query}"));
                if self.script_filter.config_index != Some(config_index) {
                    return Task::none();
                }
                self.script_filter.generation = self.script_filter.generation.wrapping_add(1);
                self.script_filter.loading = false;
                self.script_filter.results = Some(result);
                self.selected_index = self.keyword_suggestions.len() + row;
                self.handle_script_filter_selected(row)
            }
        }
    }
}
