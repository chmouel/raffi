//! The browser history keyword list: filters, loading, views, site icons
//! and refreshes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use iced::Task;

use super::browser_data::off_thread;
use super::browser_history::{self, BrowserHistoryState, Filters, Library, RowOptions};
use super::command_list::CommandListAddon;
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
        self.command_list.enter(CommandListAddon::BrowserHistory);
        if self.browser_history.session != self.command_list.session {
            self.browser_history = BrowserHistoryState {
                session: self.command_list.session,
                ..BrowserHistoryState::default()
            };
        }
        self.browser_history.query = query;
        self.browser_history.search_generation =
            self.browser_history.search_generation.wrapping_add(1);
        let debounce =
            self.browser_history.library.is_some() && !self.browser_history.query.trim().is_empty();
        self.browser_history.search_pending = debounce;
        if debounce {
            return self.schedule_browser_history_search();
        }
        if self.browser_history.library.is_some() {
            self.show_browser_history();
            return Task::none();
        }
        let filters =
            browser_history::parse_filters(&self.browser_history.query, Local::now().timestamp());
        self.command_list.set_query(filters.text);
        if self.command_list.loading || self.command_list.refreshing {
            return Task::none();
        }
        let refresh = !self.command_list.items.is_empty();
        self.load_browser_history(refresh)
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
        self.show_browser_history();
        Task::none()
    }

    /// Read history again; `refresh` keeps the rows meanwhile.
    pub(super) fn load_browser_history(&mut self, refresh: bool) -> Task<Message> {
        let generation = if refresh {
            self.command_list.begin_refresh()
        } else {
            self.command_list.begin_load()
        };
        let config = self.addons.browser_history.clone();
        Task::perform(
            off_thread(move || Arc::new(browser_history::load(&config))),
            move |library| Message::BrowserHistoryLoaded(generation, library),
        )
    }

    /// Build the rows when the view or filters changed, then set the free
    /// text for fuzzy matching.
    fn show_browser_history(&mut self) {
        self.browser_history.search_pending = false;
        let Some(library) = self.browser_history.library.clone() else {
            return;
        };
        let filters =
            browser_history::parse_filters(&self.browser_history.query, Local::now().timestamp());
        let built = (self.browser_history.view, filter_key(&filters));
        if self.browser_history.built.as_ref() != Some(&built) {
            let rows =
                browser_history::rows(&library, built.0, &filters, self.browser_history_options());
            self.browser_history.built = Some(built);
            self.command_list.usage = rows.usage;
            self.command_list.query.clear();
            self.set_command_list_items(rows.items);
        }
        self.command_list.set_query(filters.text);
    }

    /// Show the rows again after the library changed, keeping the selected
    /// row.
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

    pub(super) fn handle_browser_history_loaded(
        &mut self,
        generation: u64,
        library: Option<Arc<Library>>,
    ) -> Task<Message> {
        if !self.command_list.is_current(generation) {
            return Task::none();
        }
        let Some(library) = library else {
            self.command_list
                .set_notice("Loading browser history stopped unexpectedly".to_string());
            return Task::none();
        };
        self.browser_history.library = Some(library.clone());
        self.reshow_browser_history();
        self.lookup_browser_history_icons(library, true)
    }

    fn lookup_browser_history_icons(&self, library: Arc<Library>, fetch: bool) -> Task<Message> {
        let session = self.browser_history.session;
        Task::perform(
            off_thread(move || browser_history::lookup_icons(&library)),
            move |icons| {
                Message::BrowserHistoryIconsLoaded(session, icons.unwrap_or_default(), fetch)
            },
        )
    }

    /// Show the site icons found, then download the missing ones once.
    pub(super) fn handle_browser_history_icons_loaded(
        &mut self,
        session: u64,
        icons: HashMap<String, PathBuf>,
        fetch: bool,
    ) -> Task<Message> {
        if !self.browser_history_session_is(session) {
            return Task::none();
        }
        if let Some(library) = &mut self.browser_history.library {
            if !icons.is_empty() && library.icons != icons {
                Arc::make_mut(library).icons = icons;
                self.reshow_browser_history();
            }
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
        match &self.browser_history.library {
            Some(library) if self.browser_history_session_is(session) => {
                self.lookup_browser_history_icons(library.clone(), false)
            }
            _ => Task::none(),
        }
    }

    fn browser_history_session_is(&self, session: u64) -> bool {
        self.command_list.addon == Some(CommandListAddon::BrowserHistory)
            && self.command_list.session == session
            && self.browser_history.session == session
    }

    /// Ctrl+Tab: Recent, Frequent, By browser.
    pub(super) fn handle_browser_history_cycle_view(&mut self, backwards: bool) -> Task<Message> {
        self.browser_history.view = self.browser_history.view.cycle(backwards);
        self.browser_history.built = None;
        self.selected_index = self.command_list_offset();
        self.show_browser_history();
        Task::none()
    }
}
