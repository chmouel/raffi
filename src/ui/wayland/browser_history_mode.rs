//! The browser history keyword list: browser views, lazy loading, filters,
//! site icons and refreshes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use iced::Task;

use super::browser_data::off_thread;
use super::browser_history::{self, BrowserHistoryState, Filters, Library, RowOptions, View};
use super::command_list::{CommandItem, CommandListAddon};
use super::state::LauncherApp;
use super::types::Message;

/// Wait after the last key before rebuilding the history results.
pub(super) const SEARCH_DEBOUNCE: Duration = Duration::from_secs(1);

fn filter_key(filters: &Filters) -> Filters {
    let mut key = filters.clone();
    key.text.clear();
    key
}

impl LauncherApp {
    pub(super) fn browser_history_options(&self) -> RowOptions {
        RowOptions {
            keep_open: !self.should_close(self.addons.browser_history.close_on_action),
        }
    }

    pub(super) fn handle_browser_history_query(&mut self, query: String) -> Task<Message> {
        let handoff = self.command_list.addon == Some(CommandListAddon::BrowserHistory)
            && !self.command_list.items.is_empty()
            && self.browser_history.session != self.command_list.session;
        self.command_list.enter(CommandListAddon::BrowserHistory);
        if self.browser_history.session != self.command_list.session {
            self.browser_history = BrowserHistoryState {
                session: self.command_list.session,
                handoff,
                ..BrowserHistoryState::default()
            };
        }
        self.browser_history.query = query;
        self.browser_history.search_generation =
            self.browser_history.search_generation.wrapping_add(1);
        let loaded = self.browser_history.active_library().is_some();
        let debounce = loaded && !self.browser_history.query.trim().is_empty();
        self.browser_history.search_pending = debounce;
        if debounce {
            return self.schedule_browser_history_search();
        }
        if loaded {
            self.show_browser_history();
            return Task::none();
        }
        let filters =
            browser_history::parse_filters(&self.browser_history.query, Local::now().timestamp());
        if self.command_list.loading || self.command_list.refreshing {
            return Task::none();
        }
        if !self.browser_history.initialized {
            if self.browser_history.handoff {
                self.command_list.set_query(filters.text);
                return Task::none();
            }
            return self.load_browser_history_init();
        }
        if self.browser_history.groups.is_empty() {
            self.set_browser_history_empty_notice();
            return Task::none();
        }
        self.load_browser_history(false)
    }

    fn schedule_browser_history_search(&self) -> Task<Message> {
        let session = self.browser_history.session;
        let generation = self.browser_history.search_generation;
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    std::thread::sleep(SEARCH_DEBOUNCE);
                    let _ = sender.send(());
                });
                let _ = receiver.await;
                (session, generation)
            },
            move |(session, generation)| Message::BrowserHistorySearchDue(session, generation),
        )
    }

    /// Apply the query without waiting for the debounce, for Enter or a
    /// pointer click.
    pub(super) fn flush_browser_history_search(&mut self) -> bool {
        if !self.browser_history.search_pending {
            return false;
        }
        self.show_browser_history();
        self.selected_index = self.command_list_offset();
        true
    }

    pub(super) fn handle_browser_history_search_due(
        &mut self,
        session: u64,
        generation: u64,
    ) -> Task<Message> {
        if !self.browser_history_session_is(session)
            || self.browser_history.search_generation != generation
            || !self.browser_history.search_pending
        {
            return Task::none();
        }
        // The selection may have moved through the old rows meanwhile.
        self.show_browser_history();
        self.selected_index = self.command_list_offset();
        self.snap_to_selection()
    }

    /// Discover every browser view and load only the first one.
    fn load_browser_history_init(&mut self) -> Task<Message> {
        let generation = self.command_list.begin_load();
        let config = self.addons.browser_history.clone();
        Task::perform(
            off_thread(move || Arc::new(browser_history::initialize(&config))),
            move |init| Message::BrowserHistoryInitialized(generation, init),
        )
    }

    pub(super) fn handle_browser_history_initialized(
        &mut self,
        generation: u64,
        init: Option<Arc<browser_history::BrowserHistoryInit>>,
    ) -> Task<Message> {
        if !self.command_list.is_current(generation) {
            return Task::none();
        }
        let Some(init) = init else {
            self.command_list
                .set_notice("Loading browser history stopped unexpectedly".to_string());
            return Task::none();
        };
        let init = Arc::try_unwrap(init).unwrap_or_else(|init| (*init).clone());
        self.browser_history.groups = init.groups;
        self.browser_history.discovery_errors = init.errors;
        self.browser_history.libraries = vec![None; self.browser_history.groups.len()];
        if let Some(library) = init.library {
            if let Some(slot) = self.browser_history.libraries.first_mut() {
                *slot = Some(Arc::new(library));
            }
        }
        self.browser_history.initialized = true;
        self.browser_history.handoff = false;
        self.browser_history.active = 0;
        self.browser_history.built = None;
        if self.browser_history.groups.is_empty() {
            self.set_browser_history_empty_notice();
            return Task::none();
        }
        self.reshow_browser_history();
        self.lookup_active_browser_history_icons(true)
    }

    fn set_browser_history_empty_notice(&mut self) {
        let notice = if self.browser_history.discovery_errors.is_empty() {
            "No browser history found".to_string()
        } else {
            self.browser_history.discovery_errors.join("; ")
        };
        self.command_list.set_notice(notice);
    }

    /// Read the active browser again; `refresh` keeps its rows meanwhile.
    pub(super) fn load_browser_history(&mut self, refresh: bool) -> Task<Message> {
        if !self.browser_history.initialized {
            return self.load_browser_history_init();
        }
        let Some(group) = self.browser_history.active_group().cloned() else {
            return Task::none();
        };
        let key = group.key.clone();
        let generation = if refresh {
            self.command_list.begin_refresh()
        } else {
            self.command_list.begin_load()
        };
        let config = self.addons.browser_history.clone();
        Task::perform(
            off_thread(move || Arc::new(browser_history::load_group(&config, &group, refresh))),
            move |library| Message::BrowserHistoryGroupLoaded(generation, key, library),
        )
    }

    pub(super) fn handle_browser_history_group_loaded(
        &mut self,
        generation: u64,
        key: String,
        library: Option<Arc<Library>>,
    ) -> Task<Message> {
        if !self.command_list.is_current(generation)
            || self.browser_history.active_group().map(|group| &group.key) != Some(&key)
        {
            return Task::none();
        }
        let Some(library) = library else {
            self.command_list
                .set_notice("Loading browser history stopped unexpectedly".to_string());
            return Task::none();
        };
        if let Some(slot) = self
            .browser_history
            .libraries
            .get_mut(self.browser_history.active)
        {
            *slot = Some(library.clone());
        }
        self.reshow_browser_history();
        self.lookup_browser_history_icons(key, library, true)
    }

    fn lookup_active_browser_history_icons(&self, fetch: bool) -> Task<Message> {
        let Some(group) = self.browser_history.active_group() else {
            return Task::none();
        };
        let Some(library) = self.browser_history.active_library() else {
            return Task::none();
        };
        self.lookup_browser_history_icons(group.key.clone(), library, fetch)
    }

    fn lookup_browser_history_icons(
        &self,
        key: String,
        library: Arc<Library>,
        fetch: bool,
    ) -> Task<Message> {
        let session = self.browser_history.session;
        Task::perform(
            off_thread(move || browser_history::lookup_icons(&library)),
            move |icons| {
                Message::BrowserHistoryIconsLoaded(session, key, icons.unwrap_or_default(), fetch)
            },
        )
    }

    /// Show the site icons found, then download the missing ones once.
    pub(super) fn handle_browser_history_icons_loaded(
        &mut self,
        session: u64,
        key: String,
        icons: HashMap<String, PathBuf>,
        fetch: bool,
    ) -> Task<Message> {
        if !self.browser_history_session_is(session) {
            return Task::none();
        }
        let active = self.browser_history.active;
        let index = self
            .browser_history
            .groups
            .iter()
            .position(|group| group.key == key);
        let mut changed = false;
        if let Some(index) = index {
            if let Some(Some(library)) = self.browser_history.libraries.get_mut(index) {
                if !icons.is_empty() && library.icons != icons {
                    Arc::make_mut(library).icons = icons;
                    changed = true;
                }
            }
        }
        if changed && index == Some(active) {
            self.reshow_browser_history();
        }
        if !fetch {
            return Task::none();
        }
        let service = self.addons.browser_tabs.favicon_service;
        Task::perform(
            off_thread(move || super::favicons::fetch_pending(service)),
            move |fetched| Message::FaviconsFetched(session, fetched.unwrap_or(false)),
        )
    }

    /// Look the icons up again after some were downloaded.
    pub(super) fn handle_browser_history_favicons_fetched(
        &mut self,
        session: u64,
    ) -> Task<Message> {
        if !self.browser_history_session_is(session) {
            return Task::none();
        }
        self.lookup_active_browser_history_icons(false)
    }

    fn browser_history_session_is(&self, session: u64) -> bool {
        self.command_list.addon == Some(CommandListAddon::BrowserHistory)
            && self.command_list.session == session
            && self.browser_history.session == session
    }

    /// Build the rows when the active browser or filters changed, then set the
    /// free text for matching.
    fn show_browser_history(&mut self) {
        self.browser_history.search_pending = false;
        let Some(library) = self.browser_history.active_library() else {
            return;
        };
        let filters =
            browser_history::parse_filters(&self.browser_history.query, Local::now().timestamp());
        let built = (self.browser_history.active, filter_key(&filters));
        if self.browser_history.built.as_ref() != Some(&built) {
            let mut rows = browser_history::rows(
                &library,
                View::ByBrowser,
                &filters,
                self.browser_history_options(),
            );
            if rows.items.iter().all(|item| item.primary.is_none()) {
                for (index, error) in self.browser_history.discovery_errors.iter().enumerate() {
                    rows.items.push(CommandItem::info(
                        &format!("history-discovery-error:{index}"),
                        error.clone(),
                        "dialog-error",
                    ));
                }
            }
            self.browser_history.built = Some(built);
            self.command_list.usage = rows.usage;
            self.command_list.query.clear();
            self.set_command_list_items(rows.items);
        }
        self.command_list.set_query(filters.text);
    }

    /// Show the rows again after a library changed, keeping the selected row.
    fn reshow_browser_history(&mut self) {
        let selected = self
            .command_list
            .item(self.selected_index)
            .map(|item| item.identity.clone());
        self.browser_history.built = None;
        self.show_browser_history();
        self.selected_index = selected
            .and_then(|identity| self.command_list.position(&identity))
            .unwrap_or(0);
    }

    /// Ctrl+Tab: cycle through the browser views.
    pub(super) fn handle_browser_history_cycle_view(&mut self, backwards: bool) -> Task<Message> {
        if self.browser_history.groups.is_empty() {
            return self.load_browser_history(false);
        }
        if self.browser_history.groups.len() < 2 {
            return Task::none();
        }
        let count = self.browser_history.groups.len();
        self.browser_history.active = if backwards {
            (self.browser_history.active + count - 1) % count
        } else {
            (self.browser_history.active + 1) % count
        };
        self.browser_history.built = None;
        self.browser_history.search_pending = false;
        self.browser_history.search_generation =
            self.browser_history.search_generation.wrapping_add(1);
        self.selected_index = self.command_list_offset();
        if self.browser_history.active_library().is_some() {
            self.show_browser_history();
            Task::none()
        } else {
            self.load_browser_history(false)
        }
    }
}
