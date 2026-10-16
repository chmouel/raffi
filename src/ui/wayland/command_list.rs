use std::process::Command;
use std::time::Duration;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::keyboard::Modifiers;

use super::actions::{run_bounded, DeferredAction};

const KEPT_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Keyword list addons sharing the command list rows, selection and
/// confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandListAddon {
    System,
    Windows,
    Media,
}

impl CommandListAddon {
    /// Provider key for in-flight actions and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Windows => "windows",
            Self::Media => "media",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Windows => "Window",
            Self::Media => "Media",
        }
    }

    pub fn loading_text(self) -> &'static str {
        match self {
            Self::System => "Loading...",
            Self::Windows => "Loading windows...",
            Self::Media => "Loading players...",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CommandAction {
    pub action: DeferredAction,
    /// Completes "Press Enter again to ..." when the action needs a second
    /// Enter.
    pub confirm: Option<String>,
    /// Run right away with Raffi kept open, then reload the list.
    pub keep_open: bool,
}

#[derive(Debug, Clone)]
pub(super) struct CommandItem {
    pub title: String,
    pub subtitle: String,
    /// Text the query is matched against. Rows without one, such as errors,
    /// show whatever the query.
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
    pub details: Vec<(String, String)>,
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

#[derive(Default)]
pub(super) struct CommandListState {
    pub addon: Option<CommandListAddon>,
    pub items: Vec<CommandItem>,
    pub filtered: Vec<usize>,
    pub query: String,
    pub matcher: SkimMatcherV2,
    pub loading: bool,
    /// Single row replacing the list: an unsupported session, a missing
    /// tool or a failed load.
    pub notice: Option<String>,
    /// Target, action and modifiers armed by a first Enter.
    pub armed: Option<Armed>,
    /// A reload keeping the current rows until the new ones arrive.
    pub refreshing: bool,
    /// Error row from an action, kept above the rows of a refresh started
    /// before it failed.
    pub error: Option<CommandItem>,
    /// Bumped by every load and when leaving the mode, so stale loads are
    /// dropped.
    pub generation: u64,
    /// Bumped when leaving the mode, so actions finishing after the list was
    /// left alone, even when it was opened again.
    pub session: u64,
}

impl CommandListState {
    pub fn is_active(&self) -> bool {
        self.addon.is_some()
    }

    pub fn clear(&mut self) {
        self.addon = None;
        self.items.clear();
        self.filtered.clear();
        self.query.clear();
        self.loading = false;
        self.notice = None;
        self.armed = None;
        self.refreshing = false;
        self.error = None;
        self.generation = self.generation.wrapping_add(1);
        self.session = self.session.wrapping_add(1);
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
        } else if self.loading || self.notice.is_some() {
            1
        } else {
            self.filtered.len()
        }
    }

    /// The text of the loading or notice row, when one replaces the list.
    pub fn status(&self) -> Option<&str> {
        let addon = self.addon?;
        if self.loading {
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
        self.query = query;
        self.armed = None;
        self.refilter();
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
        self.loading = false;
        self.refreshing = false;
        self.notice = None;
        self.armed = None;
        self.refilter();
    }

    pub fn set_notice(&mut self, notice: String) {
        self.items.clear();
        self.filtered.clear();
        self.loading = false;
        self.refreshing = false;
        self.notice = Some(notice);
        self.armed = None;
    }

    /// Show the error of an action run with Raffi kept open above the
    /// other rows, until a later action succeeds.
    pub fn show_error(&mut self, item: CommandItem) {
        self.items
            .retain(|existing| existing.identity != item.identity);
        self.items.insert(0, item.clone());
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
        self.filtered.clear();
        self.loading = true;
        self.refreshing = false;
        self.error = None;
        self.notice = None;
        self.armed = None;
        self.generation
    }

    /// Start reloading after a successful action while the current rows
    /// stay usable. Earlier errors are dropped once it lands.
    pub fn begin_refresh(&mut self) -> u64 {
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
        filter_items_into(&self.items, &self.query, &self.matcher, &mut self.filtered);
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
    let DeferredAction::Command(argv) = action else {
        return Err("this action only runs after Raffi closes".to_string());
    };
    let (program, args) = argv.split_first().ok_or("no command to run")?;
    let mut command = Command::new(program);
    command.args(args);
    run_bounded(command, KEPT_OPEN_TIMEOUT, 64 * 1024)
        .map(|_| ())
        .map_err(|error| format!("{program}: {error}"))
}

pub(super) fn filter_items_into(
    items: &[CommandItem],
    query: &str,
    matcher: &SkimMatcherV2,
    out: &mut Vec<usize>,
) {
    out.clear();
    if query.is_empty() {
        out.extend(0..items.len());
        return;
    }
    let mut scored: Vec<(usize, i64)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            if item.match_text.is_empty() {
                return Some((index, i64::MAX));
            }
            matcher
                .fuzzy_match(&item.match_text, query)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    out.extend(scored.iter().map(|&(index, _)| index));
}
