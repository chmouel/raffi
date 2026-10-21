//! The Slack keyword list: switches between the home, search, unread,
//! threads, activity, actions and people views as the query changes,
//! debounces searches, loads previews and keeps usage counts.

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
pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_millis(350);
pub(super) const MIN_SEARCH_CHARS: usize = 2;
/// Wait for the rows in view to settle before downloading their pictures.
const IMAGES_SETTLE: Duration = Duration::from_millis(150);
/// Rows around the scroll position and the selection whose pictures are
/// downloaded.
const IMAGE_WINDOW: usize = 20;
/// Rows around the scroll position and the selection whose icons are
/// drawn; iced decodes every image it lays out, so farther rows get a
/// blank of the same size.
pub(super) const ICON_WINDOW: usize = 40;

#[derive(Debug, Default)]
pub(super) struct SlackState {
    /// The view of the list session `session`, None until it loads.
    pub view: Option<View>,
    pub session: u64,
    pub usage: HashMap<String, MruEntry>,
    usage_path: Option<PathBuf>,
    pub usage_loaded: bool,
    /// Rows whose preview came from the disk cache and is being refreshed.
    stale_previews: HashSet<String>,
    /// The account epoch the previews shown belong to.
    pub epoch: u64,
    /// The last preview request, and the first one of the current epoch.
    pub preview_request: u64,
    first_request: u64,
    /// Pictures by [`slack::image_key`], None until read from disk for
    /// this session.
    images: Option<HashMap<String, String>>,
    /// Pictures asked for this session, downloaded or not.
    pub images_tried: HashSet<String>,
    /// The last picture batch, and the one being waited for.
    pub images_request: u64,
    pub images_inflight: Option<u64>,
    /// Set when Slack was not connected for a batch, until rows load again.
    pub images_blocked: bool,
    /// How far the list is scrolled, from 0 at the top to 1 at the bottom.
    pub scroll: f32,
}

impl LauncherApp {
    pub(super) fn handle_slack_query(&mut self, query: String) -> Task<Message> {
        self.command_list.enter(CommandListAddon::Slack);
        super::slack_api::set_cache_fresh(self.addons.slack.cache_fresh());
        if self.slack.session != self.command_list.session {
            self.slack.session = self.command_list.session;
            self.slack.view = None;
            self.slack.stale_previews.clear();
            self.slack.scroll = 0.0;
            self.reset_slack_images();
        }
        if !self.slack.usage_loaded {
            self.load_slack_usage();
        }
        let (view, filter) = slack::parse_view(&query);
        if self.slack.view.as_ref() == Some(&view) {
            self.command_list.set_query(filter);
            return Task::none();
        }
        self.slack.view = Some(view.clone());
        if matches!(view, View::Home | View::Actions) {
            let icon = self.resolve_icon_name(slack::SLACK_ICON);
            let action_keep_open = self.addons.slack.close_on_action != Some(true);
            self.command_list.query_row = Some(std::sync::Arc::new(move |query: &str| {
                let mut item = slack::query_row(query, icon.clone())?;
                if let Some(primary) = &mut item.primary {
                    primary.keep_open = action_keep_open;
                }
                if let Some(secondary) = &mut item.secondary {
                    secondary.keep_open = action_keep_open;
                }
                Some(item)
            }));
        } else {
            self.command_list.query_row = None;
        }
        if view == View::Home {
            self.command_list.usage = self.slack_usage();
        } else {
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
            View::Home | View::Unread | View::Threads | View::Activity | View::Actions => {
                self.load_slack(false)
            }
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
            Some(View::Threads) => {
                let cached = (!refresh).then(|| slack::cached_threads(&config)).flatten();
                self.load_slack_cached(cached, refresh, move || {
                    slack::load_threads(config, refresh)
                })
            }
            Some(View::Activity) => {
                let cached = (!refresh)
                    .then(|| slack::cached_activity(&config))
                    .flatten();
                self.load_slack_cached(cached, refresh, move || {
                    slack::load_activity(config, refresh)
                })
            }
            Some(View::Actions) => {
                let cached = (!refresh).then(slack::cached_actions).flatten();
                self.load_slack_cached(cached, refresh, move || {
                    slack::load_actions(config, refresh)
                })
            }
            Some(View::Search(terms)) if terms.chars().count() >= MIN_SEARCH_CHARS => {
                let refresh = refresh || !self.command_list.items.is_empty();
                self.load_command_list(move || slack::load_search(config, terms), refresh)
            }
            Some(View::People(name)) if name.chars().count() >= MIN_SEARCH_CHARS => {
                let refresh = refresh || !self.command_list.items.is_empty();
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
            None => refresh || !self.command_list.items.is_empty(),
        };
        self.load_command_list(load, refresh)
    }

    pub(super) fn apply_slack_keep_open(&self, rows: &mut [CommandItem]) {
        let open_keep_open = !self.addons.slack.close_on_action(self.close_on_action);
        let action_keep_open = self.addons.slack.close_on_action != Some(true);
        for row in rows {
            for cmd in [&mut row.primary, &mut row.secondary].into_iter().flatten() {
                match &cmd.action {
                    super::actions::DeferredAction::Command(_)
                    | super::actions::DeferredAction::Slack(
                        slack::SlackAction::Open(_)
                        | slack::SlackAction::OpenUnread { .. }
                        | slack::SlackAction::OpenThread { .. },
                    ) => cmd.keep_open = open_keep_open,
                    super::actions::DeferredAction::Slack(_) => cmd.keep_open = action_keep_open,
                    _ => {}
                }
            }
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
    /// higher next time, and drop an unread conversation or thread row from
    /// the disk cache at once.
    pub(super) fn record_slack_pick(&mut self, identity: &str) {
        if let Some(rest) = identity.strip_prefix("slack:unread:") {
            let channel = rest.split(':').next().unwrap_or_default();
            if !channel.is_empty() {
                slack::forget_cached_unread(channel);
            }
            return;
        }
        if let Some(rest) = identity.strip_prefix("slack:thread:") {
            if let [channel, _, thread_ts] = rest.split(':').collect::<Vec<_>>()[..] {
                slack::forget_cached_thread(channel, thread_ts);
            }
            return;
        }
        if !identity.starts_with("slack:conv:") && !identity.starts_with("slack:user:") {
            return;
        }
        record_mru_use(&mut self.slack.usage, identity.to_string());
        save_mru_map(self.slack.usage_path.as_deref(), &self.slack.usage);
    }

    pub(super) fn load_slack_usage(&mut self) {
        if self.slack.usage_loaded {
            return;
        }
        self.slack.usage_loaded = true;
        self.slack.usage_path = crate::ui::get_slack_cache_dir()
            .ok()
            .map(|dir| dir.join("usage.cache"));
        self.slack.usage = load_mru_map(self.slack.usage_path.as_deref());
    }

    pub(super) fn slack_usage(&self) -> HashMap<String, u64> {
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
        let newest_first = self.addons.slack.message_order == crate::SlackMessageOrder::NewestFirst;
        if !self.command_list.previews.contains_key(&identity) {
            if let Some(preview) = slack::cached_preview(&identity, newest_first) {
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
                        let preview =
                            slack::load_preview(&identity, &wanted, stale, epoch, newest_first);
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
        self.reset_slack_images();
        if self.command_list.addon == Some(CommandListAddon::Slack) {
            self.command_list.previews.clear();
            self.command_list.preview_inflight = None;
        }
    }

    fn show_slack_preview(&mut self, identity: String, (bodies, avatar): slack::Preview) {
        if let (Some(avatar), Some(user)) = (avatar, identity.strip_prefix("slack:user:")) {
            self.add_slack_images(vec![(user.to_string(), avatar)]);
        }
        self.command_list.previews.insert(identity, bodies);
    }

    /// Forget the pictures of the last session or account, and the batch
    /// downloading them.
    fn reset_slack_images(&mut self) {
        self.slack.images = None;
        self.slack.images_tried.clear();
        self.slack.images_inflight = None;
        self.slack.images_request += 1;
        self.slack.images_blocked = false;
        slack::cancel_image_downloads();
    }

    fn slack_images(&mut self) -> &HashMap<String, String> {
        self.slack.images.get_or_insert_with(slack::cached_images)
    }

    /// Show the known pictures on Slack rows about to be listed.
    pub(super) fn apply_slack_images(&mut self, items: &mut [CommandItem]) {
        self.slack.images_blocked = false;
        let directory = super::slack_api::cached_directory();
        slack::apply_images(items, directory.as_deref(), self.slack_images());
    }

    fn add_slack_images(&mut self, found: Vec<(String, String)>) {
        if found.is_empty() {
            return;
        }
        self.slack_images();
        let images = self.slack.images.get_or_insert_with(HashMap::new);
        images.extend(found);
        let directory = super::slack_api::cached_directory();
        slack::apply_images(&mut self.command_list.items, directory.as_deref(), images);
    }

    /// Whether list row `row` is within `margin` rows of the scroll
    /// position or the selection. With `relative` scrolled, the first row
    /// drawn is `relative * (count - shown)`, so any margin of at least
    /// `shown` rows around `relative * count` covers every row drawn.
    pub(super) fn slack_row_near(&self, row: usize, margin: usize) -> bool {
        let count = self.command_list.row_count();
        let scrolled = (self.slack.scroll.clamp(0.0, 1.0) * count as f32) as usize;
        let selected = self
            .selected_index
            .saturating_sub(self.command_list_offset());
        row.abs_diff(scrolled) < margin || row.abs_diff(selected) < margin
    }

    /// The identities of the rows around the scroll position and the
    /// selection.
    fn slack_rows_in_view(&self) -> Vec<&str> {
        (0..self.command_list.row_count())
            .filter(|row| self.slack_row_near(*row, IMAGE_WINDOW))
            .filter_map(|row| self.command_list.item(row))
            .map(|item| item.identity.as_str())
            .collect()
    }

    /// Pictures the rows in view lack, with the account they are for.
    fn slack_image_wants(&mut self) -> Option<(slack::ImageWants, super::slack_api::Workspace)> {
        let directory = super::slack_api::cached_directory()?;
        self.slack_images();
        let images = self.slack.images.as_ref()?;
        let wants = slack::image_wants(
            self.slack_rows_in_view(),
            &directory,
            images,
            &self.slack.images_tried,
        );
        (!wants.is_empty()).then(|| (wants, directory.workspace.clone()))
    }

    /// Start downloading the pictures the rows in view lack, one batch at
    /// a time, once the rows have settled.
    pub(super) fn ensure_slack_images(&mut self) -> Task<Message> {
        if self.command_list.addon != Some(CommandListAddon::Slack) {
            // Left Slack: stop downloading for rows no longer shown.
            if self.slack.images_inflight.is_some() {
                self.reset_slack_images();
            }
            return Task::none();
        }
        if self.slack.images_inflight.is_some()
            || self.slack.images_blocked
            || self.slack.view.is_none()
        {
            return Task::none();
        }
        if self.slack_image_wants().is_none() {
            return Task::none();
        }
        self.slack.images_request += 1;
        let request = self.slack.images_request;
        self.slack.images_inflight = Some(request);
        let session = self.command_list.session;
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    std::thread::sleep(IMAGES_SETTLE);
                    let _ = sender.send(());
                });
                let _ = receiver.await;
            },
            move |()| Message::SlackImagesDue(session, request),
        )
    }

    pub(super) fn handle_slack_images_due(&mut self, session: u64, request: u64) -> Task<Message> {
        if self.slack.images_inflight != Some(request) {
            return Task::none();
        }
        let wanted = (self.command_list.addon == Some(CommandListAddon::Slack)
            && self.command_list.session == session)
            .then(|| self.slack_image_wants())
            .flatten();
        let Some((wants, account)) = wanted else {
            self.slack.images_inflight = None;
            return Task::none();
        };
        let epoch = self.slack.epoch;
        let generation = slack::image_generation();
        let mut keys: Vec<String> = wants.users.iter().map(|(id, _)| id.clone()).collect();
        keys.extend(wants.team.clone());
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(slack::fetch_images(wants, &account, generation));
                });
                receiver
                    .await
                    .unwrap_or(slack::ImagesOutcome::Attempted(Vec::new()))
            },
            move |outcome| {
                Message::SlackImagesLoaded(session, epoch, request, keys.clone(), outcome)
            },
        )
    }

    pub(super) fn handle_slack_images_loaded(
        &mut self,
        session: u64,
        epoch: u64,
        request: u64,
        keys: Vec<String>,
        outcome: slack::ImagesOutcome,
    ) -> Task<Message> {
        self.sync_slack_account();
        // Only the batch in flight releases the mark, so one dropped by a
        // session or account change cannot release its successor's.
        if self.slack.images_inflight != Some(request) {
            return Task::none();
        }
        self.slack.images_inflight = None;
        if self.command_list.addon != Some(CommandListAddon::Slack)
            || self.command_list.session != session
            || self.slack.epoch != epoch
        {
            return Task::none();
        }
        match outcome {
            slack::ImagesOutcome::Skipped => self.slack.images_blocked = true,
            slack::ImagesOutcome::Attempted(found) => {
                self.slack.images_tried.extend(keys);
                self.add_slack_images(found);
            }
        }
        Task::none()
    }

    pub(super) fn handle_slack_preview_open(
        &mut self,
        desktop_url: Option<String>,
        browser_url: Option<String>,
    ) -> Task<Message> {
        let url = match self.addons.slack.open_in {
            crate::SlackOpenIn::Desktop => desktop_url.or(browser_url),
            crate::SlackOpenIn::Browser => browser_url.or(desktop_url),
        };
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return Task::none();
        };
        let identity = self
            .selected_index
            .checked_sub(self.command_list_offset())
            .and_then(|row| self.command_list.item(row))
            .map(|item| item.identity.clone())
            .unwrap_or_else(|| format!("slack:open:{url}"));
        let action = slack::open_action(url);
        let keep_open = !self.addons.slack.close_on_action(self.close_on_action);
        self.run_command_list_action(CommandListAddon::Slack, identity, action, keep_open)
    }
}
