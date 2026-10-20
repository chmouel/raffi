//! The Slack keyword list: switches between the home, search and unread
//! views as the query changes, debounces searches, loads previews and keeps
//! usage counts.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use iced::widget::operation::move_cursor_to_end;
use iced::Task;

use super::command_list::{CommandItem, CommandListAddon};
use super::slack::{self, View};
use super::state::LauncherApp;
use super::support::{
    load_mru_map, mru_bounds, mru_sort_key, record_mru_use, save_mru_map, MruEntry,
};
use super::types::Message;

/// Wait after the last key before searching, so typing does not send one
/// search per key.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(350);
const MIN_SEARCH_CHARS: usize = 2;

#[derive(Debug, Default)]
pub(super) struct SlackState {
    /// The view of the list session `session`, None until it loads.
    pub view: Option<View>,
    pub session: u64,
    usage: HashMap<String, MruEntry>,
    usage_path: Option<PathBuf>,
    usage_loaded: bool,
    /// Rows whose preview came from the disk cache and is being refreshed.
    stale_previews: HashSet<String>,
    /// The account epoch the previews shown belong to.
    pub epoch: u64,
    /// The last preview request, and the first one of the current epoch.
    pub preview_request: u64,
    first_request: u64,
}

impl LauncherApp {
    pub(super) fn handle_slack_query(&mut self, query: String) -> Task<Message> {
        self.command_list.enter(CommandListAddon::Slack);
        super::slack_api::set_cache_fresh(self.addons.slack.cache_fresh());
        if self.slack.session != self.command_list.session {
            self.slack.session = self.command_list.session;
            self.slack.view = None;
            self.slack.stale_previews.clear();
        }
        if !self.slack.usage_loaded {
            self.slack.usage_loaded = true;
            self.slack.usage_path = crate::ui::get_slack_cache_dir()
                .ok()
                .map(|dir| dir.join("usage.cache"));
            self.slack.usage = load_mru_map(self.slack.usage_path.as_deref());
        }
        let (view, filter) = slack::parse_view(&query);
        if self.slack.view.as_ref() == Some(&view) {
            self.command_list.set_query(filter);
            return Task::none();
        }
        self.slack.view = Some(view.clone());
        if view == View::Home {
            let icon = self.resolve_icon_name(slack::SLACK_ICON);
            self.command_list.query_row = Some(std::sync::Arc::new(move |query: &str| {
                slack::query_row(query, icon.clone())
            }));
            self.command_list.usage = self.slack_usage();
        } else {
            self.command_list.query_row = None;
            self.command_list.usage.clear();
        }
        self.command_list.set_query(filter);
        let people = matches!(view, View::People(_));
        let searched = slack::cached_search(&view);
        match view {
            View::Search(terms) | View::People(terms)
                if terms.chars().count() < MIN_SEARCH_CHARS =>
            {
                self.slack_hint(if people {
                    "Type the name to look for"
                } else {
                    "Type the words to search for"
                });
                Task::none()
            }
            View::Search(_) | View::People(_) if searched.is_some() => {
                self.command_list.begin_load();
                self.set_command_list_items(searched.unwrap_or_default());
                Task::none()
            }
            View::Search(terms) | View::People(terms) => {
                self.slack_hint(&if people {
                    format!("Looking for \u{201c}{terms}\u{201d}...")
                } else {
                    format!("Searching messages for \u{201c}{terms}\u{201d}...")
                });
                let session = self.command_list.session;
                Task::perform(
                    async move {
                        let (sender, receiver) = iced::futures::channel::oneshot::channel();
                        std::thread::spawn(move || {
                            std::thread::sleep(SEARCH_DEBOUNCE);
                            let _ = sender.send(());
                        });
                        let _ = receiver.await;
                        terms
                    },
                    move |terms| Message::SlackSearchDue(session, terms),
                )
            }
            View::Home | View::Unread => self.load_slack(false),
        }
    }

    /// Search once the terms have stayed the same for the debounce.
    pub(super) fn handle_slack_search_due(&mut self, session: u64, terms: String) -> Task<Message> {
        let current = self.command_list.addon == Some(CommandListAddon::Slack)
            && self.command_list.session == session
            && self.slack.view.as_ref().and_then(View::terms) == Some(terms.as_str());
        if !current {
            return Task::none();
        }
        self.load_slack(false)
    }

    /// Load the rows of the current view; a refresh keeps the rows shown
    /// until it lands.
    pub(super) fn load_slack(&mut self, refresh: bool) -> Task<Message> {
        let config = self.addons.slack.clone();
        match self.slack.view.clone() {
            Some(View::Home) => {
                let cached = (!refresh).then(|| slack::cached_home(&config)).flatten();
                self.load_slack_cached(cached, refresh, move || slack::load_home(config, refresh))
            }
            Some(View::Unread) => {
                let cached = (!refresh).then(|| slack::cached_unread(&config)).flatten();
                self.load_slack_cached(cached, refresh, move || slack::load_unread(config, refresh))
            }
            Some(View::Search(terms)) if terms.chars().count() >= MIN_SEARCH_CHARS => {
                self.load_command_list(move || slack::load_search(config, terms), refresh)
            }
            Some(View::People(name)) if name.chars().count() >= MIN_SEARCH_CHARS => {
                self.load_command_list(move || slack::load_people(config, name, refresh), refresh)
            }
            _ => Task::none(),
        }
    }

    /// Re-fetch the current Slack view and its previews from Slack.
    pub(super) fn refresh_slack(&mut self) -> Task<Message> {
        self.slack
            .stale_previews
            .extend(self.command_list.previews.keys().cloned());
        self.command_list.preview_inflight = None;
        self.load_slack(true).chain(self.ensure_slack_preview())
    }

    /// Show the cached rows, if any, while loading the fresh ones.
    fn load_slack_cached(
        &mut self,
        cached: Option<Vec<CommandItem>>,
        refresh: bool,
        load: impl FnOnce() -> Result<Vec<CommandItem>, String> + Send + 'static,
    ) -> Task<Message> {
        let refresh = match cached {
            Some(rows) => {
                self.command_list.begin_load();
                self.set_command_list_items(rows);
                true
            }
            None => refresh,
        };
        self.load_command_list(load, refresh)
    }

    fn slack_hint(&mut self, text: &str) {
        let item = CommandItem::info("slack:hint", text.to_string(), "edit-find");
        self.set_command_list_items(vec![item]);
    }

    /// Put `text` after the keyword, as when picking "Set status".
    pub(super) fn fill_slack_query(&mut self, text: &str) -> Task<Message> {
        let query = format!("{} {text}", self.addons.slack.trigger());
        self.search_query = query.clone();
        let input_id = self.view.search_input_id.clone();
        self.update(Message::SearchChanged(query))
            .chain(move_cursor_to_end(input_id))
    }

    /// Count a conversation or person opened from the list, so it sorts
    /// higher next time, and drop an unread row from the disk cache at once.
    pub(super) fn record_slack_pick(&mut self, identity: &str) {
        if let Some(rest) = identity.strip_prefix("slack:unread:") {
            let channel = rest.split(':').next().unwrap_or_default();
            if !channel.is_empty() {
                slack::forget_cached_unread(channel);
            }
            return;
        }
        if !identity.starts_with("slack:conv:") && !identity.starts_with("slack:user:") {
            return;
        }
        record_mru_use(&mut self.slack.usage, identity.to_string());
        save_mru_map(self.slack.usage_path.as_deref(), &self.slack.usage);
    }

    fn slack_usage(&self) -> HashMap<String, u64> {
        let map = &self.slack.usage;
        let (max_count, min_ts, max_ts) = mru_bounds(map);
        map.keys()
            .map(|key| {
                let score = mru_sort_key(key, map, &self.sort_mode, max_count, min_ts, max_ts);
                (key.clone(), score)
            })
            .collect()
    }

    /// Load the preview of the selected row, one at a time.
    pub(super) fn ensure_slack_preview(&mut self) -> Task<Message> {
        self.sync_slack_account();
        let identity = self
            .selected_index
            .checked_sub(self.command_list_offset())
            .and_then(|row| self.command_list.item(row))
            .map(|item| item.identity.clone())
            .filter(|identity| slack::has_preview(identity));
        let Some(identity) = identity else {
            self.command_list.preview_wanted.clear();
            return Task::none();
        };
        self.command_list.preview_wanted.set(&identity);
        if !self.command_list.previews.contains_key(&identity) {
            if let Some(preview) = slack::cached_preview(&identity) {
                self.show_slack_preview(identity.clone(), preview);
                self.slack.stale_previews.insert(identity.clone());
            }
        }
        let stale = self.slack.stale_previews.contains(&identity);
        let list = &mut self.command_list;
        if list.preview_inflight.is_some() || (list.previews.contains_key(&identity) && !stale) {
            return Task::none();
        }
        list.preview_inflight = Some(identity.clone());
        let wanted = list.preview_wanted.clone();
        let session = list.session;
        self.slack.preview_request += 1;
        let request = self.slack.preview_request;
        let epoch = self.slack.epoch;
        Task::perform(
            {
                let identity = identity.clone();
                async move {
                    let (sender, receiver) = iced::futures::channel::oneshot::channel();
                    std::thread::spawn(move || {
                        let preview = slack::load_preview(&identity, &wanted, stale, epoch);
                        let _ = sender.send(preview);
                    });
                    receiver.await.ok().flatten()
                }
            },
            move |preview| Message::SlackPreviewLoaded(session, identity.clone(), request, preview),
        )
    }

    pub(super) fn handle_slack_preview_loaded(
        &mut self,
        session: u64,
        identity: String,
        request: u64,
        preview: Option<slack::Preview>,
    ) -> Task<Message> {
        if session != self.command_list.session {
            return Task::none();
        }
        self.sync_slack_account();
        let list = &mut self.command_list;
        // Only the request in flight releases the mark, so one dropped by an
        // account change cannot release its successor's.
        if request == self.slack.preview_request
            && list.preview_inflight.as_ref() == Some(&identity)
        {
            list.preview_inflight = None;
        }
        let Some(preview) = preview.filter(|_| request >= self.slack.first_request) else {
            return Task::none();
        };
        self.slack.stale_previews.remove(&identity);
        self.show_slack_preview(identity, preview);
        Task::none()
    }

    /// Forget the previews of the account the credentials no longer belong
    /// to, once Slack has said so.
    pub(super) fn sync_slack_account(&mut self) {
        self.sync_slack_account_to(super::slack_api::account_epoch());
    }

    pub(super) fn sync_slack_account_to(&mut self, epoch: u64) {
        if epoch == self.slack.epoch {
            return;
        }
        self.slack.epoch = epoch;
        self.slack.first_request = self.slack.preview_request + 1;
        self.slack.stale_previews.clear();
        if self.command_list.addon == Some(CommandListAddon::Slack) {
            self.command_list.previews.clear();
            self.command_list.preview_inflight = None;
        }
    }

    fn show_slack_preview(&mut self, identity: String, (bodies, avatar): slack::Preview) {
        let list = &mut self.command_list;
        if let Some(avatar) = avatar {
            for item in list
                .items
                .iter_mut()
                .filter(|item| item.identity == identity)
            {
                item.icon = Some(avatar.clone());
            }
        }
        list.previews.insert(identity, bodies);
    }
}
