use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::keyboard::Modifiers;

use super::actions::DeferredAction;

/// Keyword list addons sharing the command list rows, selection and
/// confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandListAddon {
    System,
    Windows,
}

impl CommandListAddon {
    /// Provider key for in-flight actions and logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Windows => "windows",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Windows => "Window",
        }
    }

    pub fn loading_text(self) -> &'static str {
        match self {
            Self::System => "Loading...",
            Self::Windows => "Loading windows...",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CommandAction {
    pub action: DeferredAction,
    /// Completes "Press Enter again to ..." when the action needs a second
    /// Enter.
    pub confirm: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct CommandItem {
    pub title: String,
    pub subtitle: String,
    /// Text the query is matched against.
    pub match_text: String,
    /// Icon names tried in order; resolved to `icon` on the UI thread.
    pub icon_names: Vec<String>,
    pub icon: Option<String>,
    /// Stable target identity, so a confirmation follows the target rather
    /// than the row it happens to sit on.
    pub identity: String,
    /// Why the item cannot run, shown instead of the subtitle.
    pub unavailable: Option<String>,
    pub primary: CommandAction,
    /// Run with Ctrl+Enter; the primary action when absent.
    pub secondary: Option<CommandAction>,
    pub details: Vec<(String, String)>,
}

impl CommandItem {
    pub fn action(&self, modifiers: Modifiers) -> &CommandAction {
        match &self.secondary {
            Some(secondary) if modifiers.control() => secondary,
            _ => &self.primary,
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
    /// Run the action on the target with this identity.
    Run(DeferredAction, String),
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
    /// Bumped by every load and when leaving the mode, so stale loads are
    /// dropped.
    pub generation: u64,
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
        self.generation = self.generation.wrapping_add(1);
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

    pub fn set_items(&mut self, items: Vec<CommandItem>) {
        self.items = items;
        self.loading = false;
        self.notice = None;
        self.armed = None;
        self.refilter();
    }

    pub fn set_notice(&mut self, notice: String) {
        self.items.clear();
        self.filtered.clear();
        self.loading = false;
        self.notice = Some(notice);
        self.armed = None;
    }

    /// Start loading; the returned generation tags the result.
    pub fn begin_load(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.items.clear();
        self.filtered.clear();
        self.loading = true;
        self.notice = None;
        self.armed = None;
        self.generation
    }

    /// Whether a load started at `generation` still belongs to this list.
    pub fn is_current(&self, generation: u64) -> bool {
        self.is_active() && self.loading && generation == self.generation
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
        item.action(armed.modifiers).confirm.as_deref()
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
        if item.unavailable.is_some() {
            self.armed = None;
            return Selection::Ignore;
        }
        let action = item.action(modifiers);
        let armed = Armed {
            addon,
            identity: item.identity.clone(),
            modifiers,
        };
        if action.confirm.is_some() && self.armed.as_ref() != Some(&armed) {
            self.armed = Some(armed);
            return Selection::Armed;
        }
        let run = Selection::Run(action.action.clone(), item.identity.clone());
        self.armed = None;
        run
    }
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
            matcher
                .fuzzy_match(&item.match_text, query)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    out.extend(scored.iter().map(|&(index, _)| index));
}
