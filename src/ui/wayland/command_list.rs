use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::Duration;

use fuzzy_matcher::skim::SkimMatcherV2;
use iced::keyboard::Modifiers;

use super::actions::{run_bounded, DeferredAction};
use super::ranking::{
    score_document, score_document_literal_only, MatchQuality, PreparedQuery, ScoredMatch,
    SearchDocument,
};
use super::url_match::{base_domain, query_host, uri_host};

const KEPT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Keyword list addons sharing the command list rows, selection and
/// confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandListAddon {
    System,
    Windows,
    BrowserTabs,
    Bookmarks,
    BrowserHistory,
    Media,
    Audio,
    Kill,
    Bluetooth,
    Pass,
    Bitwarden,
    Terminal,
    Slack,
}

impl CommandListAddon {
    /// Provider key for in-flight actions and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Windows => "windows",
            Self::BrowserTabs => "browser_tabs",
            Self::Bookmarks => "bookmarks",
            Self::BrowserHistory => "browser_history",
            Self::Media => "media",
            Self::Audio => "audio",
            Self::Kill => "kill",
            Self::Bluetooth => "bluetooth",
            Self::Pass => "pass",
            Self::Bitwarden => "bitwarden",
            Self::Terminal => "terminal",
            Self::Slack => "slack",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Windows => "Window",
            Self::BrowserTabs => "Tab",
            Self::Bookmarks => "Bookmark",
            Self::BrowserHistory => "History",
            Self::Media => "Media",
            Self::Audio => "Audio",
            Self::Kill => "Process",
            Self::Bluetooth => "Bluetooth",
            Self::Pass => "Password",
            Self::Bitwarden => "Bitwarden",
            Self::Terminal => "Terminal",
            Self::Slack => "Slack",
        }
    }

    pub fn loading_text(self) -> &'static str {
        match self {
            Self::System => "Loading...",
            Self::Windows => "Loading windows...",
            Self::BrowserTabs => "Loading browser tabs...",
            Self::Bookmarks => "Loading bookmarks...",
            Self::BrowserHistory => "Loading browser history...",
            Self::Media => "Loading players...",
            Self::Audio => "Loading audio devices...",
            Self::Kill => "Loading processes...",
            Self::Bluetooth => "Loading Bluetooth devices...",
            Self::Pass => "Loading password store...",
            Self::Bitwarden => "Loading Bitwarden vault...",
            Self::Terminal => "Loading...",
            Self::Slack => "Loading Slack...",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum MatchMode {
    #[default]
    Fuzzy,
    /// Literal, case-insensitive substring; matches where a term starts at a
    /// word boundary sort before matches that contain it later.
    Substring,
}

/// Separates ranked fields in `match_text` for [`MatchMode::Substring`].
/// [`CommandItem::match_text`] documents the order.
pub(super) const MATCH_FIELD_SEPARATOR: char = '\u{1f}';

#[derive(Debug, Clone)]
pub(super) struct CommandAction {
    pub action: DeferredAction,
    /// Completes "Press Enter again to ..." when the action needs a second
    /// Enter.
    pub confirm: Option<String>,
    /// Run right away with Raffi kept open, then reload the list.
    pub keep_open: bool,
    /// Shown instead of the subtitle while the action runs with Raffi kept
    /// open.
    pub progress: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct CommandItem {
    pub title: String,
    pub subtitle: String,
    /// Text the query is matched against. Rows without one, such as errors,
    /// show whatever the query. In [`MatchMode::Substring`], fields separated
    /// by [`MATCH_FIELD_SEPARATOR`] are ranked in order: URL, title, then the
    /// rest.
    pub match_text: String,
    /// Icon names tried in order; resolved to `icon` on the UI thread.
    pub icon_names: Vec<String>,
    pub icon: Option<String>,
    /// Stable target identity, so a confirmation follows the target rather
    /// than the row it happens to sit on.
    pub identity: String,
    /// Why the item cannot run, shown instead of the subtitle.
    pub unavailable: Option<String>,
    /// None for rows that only inform, such as headers and errors.
    pub primary: Option<CommandAction>,
    /// Run with Ctrl+Enter; the primary action when absent.
    pub secondary: Option<CommandAction>,
    /// Run with Ctrl+D, such as closing a window.
    pub close: Option<CommandAction>,
    pub details: Vec<(String, String)>,
    /// Short tag shown as a pill at the end of the row.
    pub badge: Option<String>,
    /// Base domains of the sites the row is for; a filter naming one of
    /// them, such as a pasted URL, lists the row first.
    pub domains: Vec<String>,
    /// Drawn in the theme's muted colour and listed last when nothing is
    /// typed, as for muted Slack conversations.
    pub dimmed: bool,
    /// Nonzero groups sort before ordinary rows in the unfiltered list.
    pub sort_priority: u8,
    /// Left out of the unfiltered list, shown once the query matches it.
    pub typed_only: bool,
    /// Section heading drawn above a run of rows sharing it while nothing
    /// is typed, such as the browser of a tab.
    pub group: Option<RowGroup>,
    /// A pinned browser tab, marked with a pin before its title.
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RowGroup {
    pub title: String,
    /// Icon name, resolved to `icon` on the UI thread.
    pub icon_name: Option<String>,
    pub icon: Option<String>,
}

impl RowGroup {
    pub fn new(title: &str, icon_name: Option<&str>) -> Self {
        Self {
            title: title.to_string(),
            icon_name: icon_name.map(str::to_string),
            icon: None,
        }
    }
}

impl CommandItem {
    pub fn action(&self, modifiers: Modifiers) -> Option<&CommandAction> {
        let primary = self.primary.as_ref()?;
        match &self.secondary {
            Some(secondary) if modifiers.control() => Some(secondary),
            _ => Some(primary),
        }
    }

    /// A row that cannot be activated, showing `title`.
    pub fn info(identity: &str, title: String, icon: &str) -> Self {
        Self {
            title,
            subtitle: String::new(),
            match_text: String::new(),
            icon_names: vec![icon.to_string()],
            icon: None,
            identity: identity.to_string(),
            unavailable: None,
            primary: None,
            secondary: None,
            details: Vec::new(),
            badge: None,
            domains: Vec::new(),
            dimmed: false,
            sort_priority: 0,
            typed_only: false,
            close: None,
            group: None,
            pinned: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Armed {
    pub addon: CommandListAddon,
    pub identity: String,
    pub modifiers: Modifiers,
}

/// What selecting a row does.
#[derive(Debug)]
pub(super) enum Selection {
    Ignore,
    Armed,
    /// Run the action on the target with this identity, keeping Raffi open
    /// when the flag is set.
    Run(DeferredAction, String, bool),
}

/// Builds a row from the query, such as the volume `vol 40` sets.
pub(super) type QueryRow = std::sync::Arc<dyn Fn(&str) -> Option<CommandItem> + Send + Sync>;

#[derive(Default)]
pub(super) struct CommandListState {
    pub addon: Option<CommandListAddon>,
    pub items: Vec<CommandItem>,
    pub documents: Vec<SearchDocument>,
    pub filtered: Vec<usize>,
    pub filtered_scores: Vec<Option<ScoredMatch>>,
    pub query: String,
    pub matcher: SkimMatcherV2,
    pub loading: bool,
    /// Single row replacing the list: an unsupported session, a missing
    /// tool or a failed load.
    pub notice: Option<String>,
    /// Target, action and modifiers armed by a first Enter.
    pub armed: Option<Armed>,
    /// Identity of the row whose Ctrl+D action is running.
    pub closing: Option<String>,
    /// A reload keeping the current rows until the new ones arrive.
    pub refreshing: bool,
    pub refresh_error: Option<String>,
    pub unavailable: bool,
    /// Error row from an action, kept above the rows of a refresh started
    /// before it failed.
    pub error: Option<CommandItem>,
    /// Bumped by every load and when leaving the mode, so stale loads are
    /// dropped.
    pub generation: u64,
    /// Bumped when leaving the mode, so actions finishing after the list was
    /// left alone, even when it was opened again.
    pub session: u64,
    /// Single row shown over everything else, such as a password prompt.
    pub overlay: Option<String>,
    /// Identities an outside search, such as rbw's notes search, found for
    /// a query; listed after the local matches while the query is the same.
    pub extra: Option<(String, HashSet<String>)>,
    /// Rows from this one on come only from `extra`.
    pub extra_start: usize,
    /// Past use of rows by identity, higher first: the order with no
    /// filter, and the tie-break between equally good matches.
    pub usage: HashMap<String, u64>,
    /// Preview bodies by page URL for the browser tab rows.
    pub previews: HashMap<String, Vec<super::preview::PreviewBody>>,
    /// The page URL whose preview is being looked up.
    pub preview_inflight: Option<String>,
    /// The page URL the preview pane wants, empty for none.
    pub preview_wanted: super::tab_preview::Wanted,
    /// Row listed first while the query asks for it; it has no match text
    /// and the identity [`QUERY_ROW`].
    pub query_row: Option<QueryRow>,
    /// Keep only rows holding every word of the query, for lists mixed
    /// into the main view where a loose fuzzy match is noise.
    pub strict: bool,
    /// Devices the audio list shows; All for every other list.
    pub audio_view: super::audio::View,
    /// Order of the browser tab list.
    pub tabs_view: super::browser_tabs::View,
    /// Whether the window switcher lists windows or workspaces.
    pub windows_view: super::windows::View,
    /// Which Slack slice this list shows when used as a `main_view` source.
    pub slack_source: Option<super::main_view::SlackSource>,
    /// Hide muted or zero-usage unpromoted Slack channels when the query is empty.
    pub slack_hide_cold_channels: bool,
}

/// Identity of the row `CommandListState::query_row` builds.
pub(super) const QUERY_ROW: &str = "query-row";

impl CommandListState {
    pub fn is_active(&self) -> bool {
        self.addon.is_some()
    }

    pub fn clear(&mut self) {
        self.addon = None;
        self.items.clear();
        self.documents.clear();
        self.filtered.clear();
        self.filtered_scores.clear();
        self.query.clear();
        self.loading = false;
        self.notice = None;
        self.refresh_error = None;
        self.unavailable = false;
        self.armed = None;
        self.closing = None;
        self.refreshing = false;
        self.error = None;
        self.overlay = None;
        self.extra = None;
        self.usage.clear();
        self.previews.clear();
        self.preview_inflight = None;
        self.preview_wanted.clear();
        self.query_row = None;
        self.audio_view = super::audio::View::All;
        self.tabs_view = super::browser_tabs::View::ByBrowser;
        self.windows_view = super::windows::View::Windows;
        self.slack_source = None;
        self.slack_hide_cold_channels = false;
        self.generation = self.generation.wrapping_add(1);
        self.session = self.session.wrapping_add(1);
    }

    /// Show the audio devices of `view` only.
    pub fn set_audio_view(&mut self, view: super::audio::View) {
        self.audio_view = view;
        self.armed = None;
        self.refilter();
    }

    /// Enter `addon`, keeping the current state when it is already active.
    /// Returns whether the mode was entered just now.
    pub fn enter(&mut self, addon: CommandListAddon) -> bool {
        if self.addon == Some(addon) {
            return false;
        }
        self.clear();
        self.addon = Some(addon);
        true
    }

    pub fn row_count(&self) -> usize {
        if !self.is_active() {
            0
        } else if self.loading || self.notice.is_some() || self.overlay.is_some() {
            1
        } else {
            self.filtered.len()
        }
    }

    /// The text of the loading or notice row, when one replaces the list.
    pub fn status(&self) -> Option<&str> {
        let addon = self.addon?;
        if let Some(overlay) = &self.overlay {
            Some(overlay)
        } else if self.loading {
            Some(addon.loading_text())
        } else {
            self.notice.as_deref()
        }
    }

    pub fn item(&self, row: usize) -> Option<&CommandItem> {
        if self.status().is_some() {
            return None;
        }
        self.filtered
            .get(row)
            .and_then(|&index| self.items.get(index))
    }

    pub fn set_query(&mut self, query: String) {
        if self
            .extra
            .as_ref()
            .is_some_and(|(searched, _)| *searched != query)
        {
            self.extra = None;
        }
        self.query = query;
        self.armed = None;
        self.refilter();
    }

    /// List the rows with these identities after the local matches of
    /// `query`, unless the query has changed since.
    pub fn set_extra(&mut self, query: String, identities: HashSet<String>) {
        if query != self.query {
            return;
        }
        self.extra = Some((query, identities));
        self.refilter();
    }

    /// Whether `row` is listed only because of the outside search.
    pub fn is_extra_row(&self, row: usize) -> bool {
        self.extra.is_some() && row >= self.extra_start
    }

    pub fn set_items(&mut self, mut items: Vec<CommandItem>) {
        match &self.error {
            Some(error) if self.refreshing => {
                items.retain(|item| item.identity != error.identity);
                items.insert(0, error.clone());
            }
            _ => self.error = None,
        }
        self.items = items;
        self.documents = self
            .items
            .iter()
            .map(SearchDocument::from_command_item)
            .collect();
        self.loading = false;
        self.refreshing = false;
        self.notice = None;
        self.refresh_error = None;
        self.unavailable = false;
        self.armed = None;
        self.refilter();
    }

    pub fn load_failed(&mut self, error: String) {
        if self.refreshing && !self.items.is_empty() {
            self.refreshing = false;
            self.refresh_error = Some(error);
        } else {
            self.set_notice(error);
        }
    }

    pub fn set_unavailable(&mut self, notice: String) {
        self.set_notice(notice);
        self.unavailable = true;
    }

    pub fn set_notice(&mut self, notice: String) {
        self.items.clear();
        self.documents.clear();
        self.filtered.clear();
        self.filtered_scores.clear();
        self.loading = false;
        self.refreshing = false;
        self.notice = Some(notice);
        self.refresh_error = None;
        self.unavailable = false;
        self.armed = None;
    }

    /// Show the error of an action run with Raffi kept open above the
    /// other rows, until a later action succeeds.
    pub fn show_error(&mut self, item: CommandItem) {
        self.items
            .retain(|existing| existing.identity != item.identity);
        self.items.insert(0, item.clone());
        self.documents = self
            .items
            .iter()
            .map(SearchDocument::from_command_item)
            .collect();
        self.error = Some(item);
        self.armed = None;
        self.refilter();
    }

    /// The row showing the item with `identity`.
    pub fn position(&self, identity: &str) -> Option<usize> {
        self.filtered
            .iter()
            .position(|&index| self.items[index].identity == identity)
    }

    /// Start loading; the returned generation tags the result.
    pub fn begin_load(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.items.clear();
        self.documents.clear();
        self.filtered.clear();
        self.filtered_scores.clear();
        self.loading = true;
        self.refreshing = false;
        self.error = None;
        self.notice = None;
        self.refresh_error = None;
        self.unavailable = false;
        self.armed = None;
        self.extra = None;
        self.generation
    }

    /// Start reloading after a successful action while the current rows
    /// stay usable. Earlier errors are dropped once it lands.
    pub fn begin_refresh(&mut self) -> u64 {
        if self.items.is_empty() {
            return self.begin_load();
        }
        self.refresh_error = None;
        self.generation = self.generation.wrapping_add(1);
        self.refreshing = true;
        self.error = None;
        self.generation
    }

    /// Whether a load started at `generation` still belongs to this list.
    pub fn is_current(&self, generation: u64) -> bool {
        self.is_active() && (self.loading || self.refreshing) && generation == self.generation
    }

    fn refilter(&mut self) {
        if let Some(build) = &self.query_row {
            self.items.retain(|item| item.identity != QUERY_ROW);
            if let Some(row) = build(&self.query) {
                self.items.insert(0, row);
            }
            self.documents = self
                .items
                .iter()
                .map(SearchDocument::from_command_item)
                .collect();
        } else if self.documents.len() != self.items.len() {
            self.documents = self
                .items
                .iter()
                .map(SearchDocument::from_command_item)
                .collect();
        }
        let mode = if self.addon == Some(CommandListAddon::BrowserHistory) {
            MatchMode::Substring
        } else {
            MatchMode::Fuzzy
        };
        filter_items_with_documents_into(
            &self.items,
            Some(&self.documents),
            &self.query,
            &self.matcher,
            &self.usage,
            self.strict,
            mode,
            &mut self.filtered,
            Some(&mut self.filtered_scores),
        );
        if self.audio_view != super::audio::View::All {
            let view = self.audio_view;
            let items = &self.items;
            let mut idx = 0;
            self.filtered.retain(|&index| {
                let keep = view.keeps(&items[index].identity);
                if !keep && idx < self.filtered_scores.len() {
                    self.filtered_scores.remove(idx);
                } else {
                    idx += 1;
                }
                keep
            });
        }
        if self.slack_hide_cold_channels && self.query.is_empty() {
            let items = &self.items;
            let usage = &self.usage;
            let mut idx = 0;
            self.filtered.retain(|&index| {
                let item = &items[index];
                let keep = if !item.identity.starts_with("slack:conv:") {
                    true
                } else {
                    !item.dimmed
                        && (item.sort_priority > 0
                            || usage.get(&item.identity).copied().unwrap_or(0) > 0)
                };
                if !keep && idx < self.filtered_scores.len() {
                    self.filtered_scores.remove(idx);
                } else {
                    idx += 1;
                }
                keep
            });
        }
        self.extra_start = self.filtered.len();
        let Some((_, identities)) = &self.extra else {
            return;
        };
        let shown: HashSet<usize> = self.filtered.iter().copied().collect();
        let extra: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(index, item)| identities.contains(&item.identity) && !shown.contains(index))
            .map(|(index, _)| index)
            .collect();
        self.filtered_scores
            .extend(std::iter::repeat_n(None, extra.len()));
        self.filtered.extend(extra);
    }

    /// The confirmation verb shown on `item` while it is armed.
    pub fn armed_verb<'a>(&self, item: &'a CommandItem) -> Option<&'a str> {
        let armed = self.armed.as_ref()?;
        if Some(armed.addon) != self.addon || armed.identity != item.identity {
            return None;
        }
        item.action(armed.modifiers)?.confirm.as_deref()
    }

    /// Select `row` with `modifiers`. Actions needing confirmation run on a
    /// second selection of the same target with the same modifiers.
    pub fn select(&mut self, row: usize, modifiers: Modifiers) -> Selection {
        let Some(addon) = self.addon else {
            return Selection::Ignore;
        };
        let Some(item) = self.item(row) else {
            return Selection::Ignore;
        };
        let Some(action) = item
            .action(modifiers)
            .filter(|_| item.unavailable.is_none())
        else {
            self.armed = None;
            return Selection::Ignore;
        };
        let armed = Armed {
            addon,
            identity: item.identity.clone(),
            modifiers,
        };
        if action.confirm.is_some() && self.armed.as_ref() != Some(&armed) {
            self.armed = Some(armed);
            return Selection::Armed;
        }
        let run = Selection::Run(
            action.action.clone(),
            item.identity.clone(),
            action.keep_open,
        );
        self.armed = None;
        run
    }
}

/// Run a command action while Raffi stays open. Blocking: run it off the UI
/// thread.
pub(super) fn run_kept_open(action: &DeferredAction) -> Result<(), String> {
    match action {
        DeferredAction::Command(argv) => {
            let (program, args) = argv.split_first().ok_or("no command to run")?;
            let mut command = Command::new(program);
            command.args(args);
            run_bounded(command, KEPT_OPEN_TIMEOUT, 64 * 1024)
                .map(|_| ())
                .map_err(|error| format!("{program}: {error}"))
        }
        DeferredAction::CommandBatch(_)
        | DeferredAction::Hypr(_)
        | DeferredAction::TerminalTab { .. } => action.run(),
        DeferredAction::Signal(target) => super::kill::send_signal(target),
        DeferredAction::Bluetooth(args) => super::bluetooth::run_action(args),
        DeferredAction::BrowserTab(action) => super::browser_tabs::run(action),
        DeferredAction::Slack(action) => super::slack::run_action(action),
        DeferredAction::Copy(text) => super::actions::copy_text_checked(text),
        DeferredAction::BrowserHostSetup { force } => {
            crate::browser_host::install_current(*force).map(|_| ())
        }
        _ => Err("this action only runs after Raffi closes".to_string()),
    }
}

#[allow(dead_code)]
pub(super) fn filter_items_into(
    items: &[CommandItem],
    query: &str,
    matcher: &SkimMatcherV2,
    usage: &HashMap<String, u64>,
    strict: bool,
    mode: MatchMode,
    out: &mut Vec<usize>,
) {
    filter_items_with_documents_into(items, None, query, matcher, usage, strict, mode, out, None);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn filter_items_with_documents_into(
    items: &[CommandItem],
    documents: Option<&[SearchDocument]>,
    query: &str,
    matcher: &SkimMatcherV2,
    usage: &HashMap<String, u64>,
    _strict: bool,
    mode: MatchMode,
    out: &mut Vec<usize>,
    mut out_scores: Option<&mut Vec<Option<ScoredMatch>>>,
) {
    out.clear();
    if let Some(scores) = out_scores.as_deref_mut() {
        scores.clear();
    }
    let used = |item: &CommandItem| usage.get(&item.identity).copied().unwrap_or(0);
    let Some(prepared) = PreparedQuery::new(query) else {
        out.extend((0..items.len()).filter(|&index| !items[index].typed_only));
        if !usage.is_empty()
            || items
                .iter()
                .any(|item| item.dimmed || item.sort_priority > 0)
        {
            // Rows without match text, such as errors, stay on top.
            out.sort_by_key(|&index| {
                let item = &items[index];
                (
                    !item.match_text.is_empty(),
                    std::cmp::Reverse(item.sort_priority),
                    item.dimmed,
                    std::cmp::Reverse(if item.sort_priority == 0 {
                        used(item)
                    } else {
                        0
                    }),
                )
            });
        }
        if let Some(scores) = out_scores {
            scores.resize(out.len(), None);
        }
        return;
    };
    let domain = query_host(query).map(|host| base_domain(&host));
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    // Rows without match text stay on top in dedicated lists; domain matches
    // precede ordinary text matches; past use breaks ties.
    let rank = |index: usize,
                item: &CommandItem|
     -> Option<(usize, u8, u32, u64, Option<ScoredMatch>)> {
        if item.match_text.is_empty() {
            return Some((index, 0, 0, used(item), None));
        }
        if domain
            .as_ref()
            .is_some_and(|domain| item.domains.contains(domain))
        {
            let scored = ScoredMatch::domain();
            return Some((index, 1, scored.score, used(item), Some(scored)));
        }
        let owned_doc;
        let doc = if let Some(doc) = documents.and_then(|docs| docs.get(index)) {
            doc
        } else {
            owned_doc = SearchDocument::from_command_item(item);
            &owned_doc
        };
        match mode {
            MatchMode::Fuzzy => {
                let scored = score_document(doc, &prepared, matcher)?;
                let bucket = match scored.quality {
                    MatchQuality::DomainMatch => 1,
                    MatchQuality::ExactPrimary => 2,
                    MatchQuality::PrimaryWordPrefix => 3,
                    MatchQuality::PrimaryMatch => 4,
                    MatchQuality::MixedFields => 5,
                    MatchQuality::SecondaryOnly => 6,
                };
                Some((index, bucket, scored.score, used(item), Some(scored)))
            }
            MatchMode::Substring => {
                if terms.is_empty() {
                    return None;
                }
                let text = item.match_text.to_lowercase();
                if !terms.iter().all(|term| text.contains(term)) {
                    return None;
                }
                let mut fields = text.split(MATCH_FIELD_SEPARATOR);
                let url = fields.next().unwrap_or_default();
                let title = fields.next().unwrap_or_default();
                let rank = if terms.iter().all(|term| word_prefix(url, term)) {
                    2
                } else if terms.iter().all(|term| word_prefix(title, term)) {
                    3
                } else if terms.iter().all(|term| url.contains(term)) {
                    4
                } else if terms.iter().all(|term| title.contains(term)) {
                    5
                } else {
                    6
                };
                let scored =
                    score_document_literal_only(doc, &prepared, matcher).or(Some(ScoredMatch {
                        quality: MatchQuality::SecondaryOnly,
                        score: 2_800,
                    }));
                Some((index, rank, 0, used(item), scored))
            }
        }
    };
    // Large lists spend tens of milliseconds in the matcher on a broad query.
    // Split the scan across cores, keeping each chunk in its original order
    // so the stable sort still makes ties deterministic.
    let threads = if query.is_empty() || items.len() < 10_000 {
        1
    } else {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(8)
    };
    let mut ranked: Vec<(usize, u8, u32, u64, Option<ScoredMatch>)> = if threads == 1 {
        items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| rank(index, item))
            .collect()
    } else {
        let chunk_size = items.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let handles: Vec<_> = items
                .chunks(chunk_size)
                .enumerate()
                .map(|(chunk_index, chunk)| {
                    let rank = &rank;
                    scope.spawn(move || {
                        let start = chunk_index * chunk_size;
                        chunk
                            .iter()
                            .enumerate()
                            .filter_map(|(offset, item)| rank(start + offset, item))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap_or_default())
                .collect()
        })
    };
    ranked.sort_by_key(|&(_, rank, score, used, _)| {
        (
            rank,
            std::cmp::Reverse(used > 0),
            std::cmp::Reverse(score),
            std::cmp::Reverse(used),
        )
    });
    out.extend(ranked.iter().map(|&(index, _, _, _, _)| index));
    if let Some(scores) = out_scores {
        scores.extend(ranked.into_iter().map(|(_, _, _, _, scored)| scored));
    }
}

/// Whether `term` appears at the start of `text` or after a non-alphanumeric
/// character. `text` and `term` are already lowercase.
fn word_prefix(text: &str, term: &str) -> bool {
    text.match_indices(term).any(|(at, _)| {
        text[..at]
            .chars()
            .next_back()
            .is_none_or(|before| !before.is_alphanumeric())
    })
}

/// Base domains of the sites in `uris`, without repeats.
pub(super) fn domains_of<'a>(uris: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut domains: Vec<String> = Vec::new();
    for domain in uris
        .into_iter()
        .filter_map(uri_host)
        .map(|host| base_domain(&host))
    {
        if !domains.contains(&domain) {
            domains.push(domain);
        }
    }
    domains
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize) -> CommandItem {
        let mut item = CommandItem::info(
            &format!("perf:{index}"),
            format!("Page {index} with a browser title"),
            "web-browser",
        );
        item.match_text = format!(
            "Page {index} with a browser title https://site{}.example.com/{index}",
            index % 500
        );
        item
    }

    #[test]
    fn test_large_filter_keeps_stable_order() {
        let items: Vec<CommandItem> = (0..10_001).map(item).collect();
        let mut usage = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            usage.insert(item.identity.clone(), index as u64);
        }
        let matcher = SkimMatcherV2::default();
        let mut first = Vec::new();
        filter_items_into(
            &items,
            "page",
            &matcher,
            &usage,
            false,
            MatchMode::Fuzzy,
            &mut first,
        );
        let mut second = Vec::new();
        filter_items_into(
            &items,
            "page",
            &matcher,
            &usage,
            false,
            MatchMode::Fuzzy,
            &mut second,
        );
        assert_eq!(first.len(), items.len());
        assert_eq!(first, second);
    }

    #[test]
    #[ignore = "manual filtering benchmark; run in release mode"]
    fn benchmark_relevance_filter_against_previous_strict_scan() {
        use fuzzy_matcher::FuzzyMatcher;
        use std::time::Instant;

        let matcher = SkimMatcherV2::default();
        let usage = HashMap::new();
        for size in [1_000, 10_000, 50_000] {
            let items: Vec<_> = (0..size).map(item).collect();
            let documents: Vec<_> = items
                .iter()
                .map(SearchDocument::from_command_item)
                .collect();
            let mut output = Vec::new();
            let mut scores = Vec::new();
            let mut timings = [Vec::new(), Vec::new()];
            for _ in 0..15 {
                let start = Instant::now();
                let rank = |index: usize, item: &CommandItem| {
                    let text = item.match_text.to_lowercase();
                    (text.contains("page") && text.contains("browser"))
                        .then(|| {
                            matcher
                                .fuzzy_match(&item.match_text, "page browser")
                                .map(|score| (index, score))
                        })
                        .flatten()
                };
                let threads = if size < 10_000 {
                    1
                } else {
                    std::thread::available_parallelism()
                        .map(|count| count.get())
                        .unwrap_or(1)
                        .min(8)
                };
                let mut old: Vec<_> = if threads == 1 {
                    items
                        .iter()
                        .enumerate()
                        .filter_map(|(index, item)| rank(index, item))
                        .collect()
                } else {
                    let chunk_size = items.len().div_ceil(threads);
                    std::thread::scope(|scope| {
                        let handles: Vec<_> = items
                            .chunks(chunk_size)
                            .enumerate()
                            .map(|(chunk_index, chunk)| {
                                let rank = &rank;
                                scope.spawn(move || {
                                    chunk
                                        .iter()
                                        .enumerate()
                                        .filter_map(|(offset, item)| {
                                            rank(chunk_index * chunk_size + offset, item)
                                        })
                                        .collect::<Vec<_>>()
                                })
                            })
                            .collect();
                        handles
                            .into_iter()
                            .flat_map(|handle| handle.join().unwrap())
                            .collect()
                    })
                };
                old.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
                timings[0].push(start.elapsed());

                let start = Instant::now();
                filter_items_with_documents_into(
                    &items,
                    Some(&documents),
                    "page browser",
                    &matcher,
                    &usage,
                    true,
                    MatchMode::Fuzzy,
                    &mut output,
                    Some(&mut scores),
                );
                timings[1].push(start.elapsed());
            }
            for samples in &mut timings {
                samples.sort();
            }
            let previous = timings[0][timings[0].len() / 2].as_secs_f64() * 1_000.0;
            let current = timings[1][timings[1].len() / 2].as_secs_f64() * 1_000.0;
            println!(
                "{size} items: previous {previous:.2} ms, relevance {current:.2} ms, ratio {:.2}x",
                current / previous
            );
        }
    }

    fn substring_item(identity: &str, title: &str, url: &str) -> CommandItem {
        let mut item = CommandItem::info(identity, title.to_string(), "web-browser");
        item.match_text =
            format!("{url}{MATCH_FIELD_SEPARATOR}{title}{MATCH_FIELD_SEPARATOR}browser profile");
        item
    }

    #[test]
    fn test_substring_mode_rejects_fuzzy_only_and_ranks_url_before_title() {
        let items = vec![
            substring_item(
                "title-substring",
                "Securing trust",
                "https://example.com/weak",
            ),
            substring_item("url-substring", "Other", "https://example.com/trust"),
            substring_item("title-prefix", "Rust docs", "https://example.com/docs"),
            substring_item("url-prefix", "Docs", "https://rust.example.com/"),
            substring_item(
                "scattered",
                "Really useful search tool",
                "https://example.com/scattered",
            ),
        ];
        let mut out = Vec::new();
        filter_items_into(
            &items,
            "rust",
            &SkimMatcherV2::default(),
            &HashMap::new(),
            false,
            MatchMode::Substring,
            &mut out,
        );
        assert_eq!(out, [3, 2, 1, 0]);
    }
}
