//! The Slack keyword list: switches between the home, search and unread
//! views as the query changes, debounces searches, loads previews and keeps
//! usage counts.

use std::collections::HashMap;
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
}

impl LauncherApp {
    pub(super) fn handle_slack_query(&mut self, query: String) -> Task<Message> {
        self.command_list.enter(CommandListAddon::Slack);
        if self.slack.session != self.command_list.session {
            self.slack.session = self.command_list.session;
            self.slack.view = None;
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
            Some(View::Home) => self.load_command_list(move || slack::load_home(config), refresh),
            Some(View::Unread) => {
                self.load_command_list(move || slack::load_unread(config), refresh)
            }
            Some(View::Search(terms)) if terms.chars().count() >= MIN_SEARCH_CHARS => {
                self.load_command_list(move || slack::load_search(config, terms), refresh)
            }
            Some(View::People(name)) if name.chars().count() >= MIN_SEARCH_CHARS => {
                self.load_command_list(move || slack::load_people(config, name), refresh)
            }
            _ => Task::none(),
        }
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
    /// higher next time.
    pub(super) fn record_slack_pick(&mut self, identity: &str) {
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
        let identity = self
            .selected_index
            .checked_sub(self.command_list_offset())
            .and_then(|row| self.command_list.item(row))
            .map(|item| item.identity.clone())
            .filter(|identity| slack::has_preview(identity));
        let list = &mut self.command_list;
        let Some(identity) = identity else {
            list.preview_wanted.clear();
            return Task::none();
        };
        list.preview_wanted.set(&identity);
        if list.preview_inflight.is_some() || list.previews.contains_key(&identity) {
            return Task::none();
        }
        list.preview_inflight = Some(identity.clone());
        let wanted = list.preview_wanted.clone();
        let session = list.session;
        Task::perform(
            {
                let identity = identity.clone();
                async move {
                    let (sender, receiver) = iced::futures::channel::oneshot::channel();
                    std::thread::spawn(move || {
                        let _ = sender.send(slack::load_preview(&identity, &wanted));
                    });
                    receiver.await.ok().flatten()
                }
            },
            move |preview| Message::SlackPreviewLoaded(session, identity.clone(), preview),
        )
    }

    pub(super) fn handle_slack_preview_loaded(
        &mut self,
        session: u64,
        identity: String,
        preview: Option<slack::Preview>,
    ) -> Task<Message> {
        let list = &mut self.command_list;
        if session != list.session {
            return Task::none();
        }
        if list.preview_inflight.as_ref() == Some(&identity) {
            list.preview_inflight = None;
        }
        let Some((bodies, avatar)) = preview else {
            return Task::none();
        };
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
        Task::none()
    }
}
