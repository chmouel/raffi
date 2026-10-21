//! The bookmarks keyword list: folder paths in the query, loading, site
//! icons, source views and the term prompt of search bookmarks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use iced::widget::operation::{focus, move_cursor_to_end};
use iced::Task;

use super::bookmarks::{
    self, keyword_query, split_query, BookmarksState, Library, RowOptions, SearchIndex,
    SearchPrompt, SearchTarget,
};
use super::command_list::CommandListAddon;
use super::state::LauncherApp;
use super::types::Message;

async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.ok()
}

impl LauncherApp {
    pub(super) fn bookmark_options(&self) -> RowOptions {
        RowOptions {
            keep_open: !self.should_close(self.addons.bookmarks.close_on_action),
        }
    }

    pub(super) fn handle_bookmarks_query(&mut self, query: String) -> Task<Message> {
        self.command_list.enter(CommandListAddon::Bookmarks);
        if self.bookmarks.session != self.command_list.session {
            self.bookmarks = BookmarksState {
                session: self.command_list.session,
                ..BookmarksState::default()
            };
        }
        self.bookmarks.query = query;
        if let Some(task) = self.open_bookmark_keyword() {
            return task;
        }
        if self.bookmarks.library.is_some() {
            self.show_bookmarks();
            return Task::none();
        }
        // Rows handed over by the main view stay until the bookmarks load.
        let filter = split_query(&self.bookmarks.query).1.to_string();
        self.command_list.set_query(filter);
        if self.command_list.loading || self.command_list.refreshing {
            return Task::none();
        }
        let refresh = !self.command_list.items.is_empty();
        self.load_bookmarks(refresh)
    }

    /// Read the bookmarks again; `refresh` keeps the rows meanwhile.
    pub(super) fn load_bookmarks(&mut self, refresh: bool) -> Task<Message> {
        let generation = if refresh {
            self.command_list.begin_refresh()
        } else {
            self.command_list.begin_load()
        };
        let config = self.addons.bookmarks.clone();
        Task::perform(
            off_thread(move || Arc::new(bookmarks::load(&config))),
            move |library| Message::BookmarksLoaded(generation, library),
        )
    }

    /// Build the rows of the query's folder when it changed, then filter
    /// them.
    fn show_bookmarks(&mut self) {
        if self.show_search_prompt() {
            return;
        }
        let Some(library) = self.bookmarks.library.clone() else {
            return;
        };
        let (path, filter) = split_query(&self.bookmarks.query);
        let (path, filter) = (path.to_string(), filter.to_string());
        let built = (path, self.bookmarks.view, !filter.is_empty());
        if self.bookmarks.built.as_ref() != Some(&built) {
            let rows = bookmarks::rows(
                &library,
                built.1,
                &built.0,
                built.2,
                self.bookmark_options(),
            );
            self.bookmarks.folders = rows.folders;
            self.bookmarks.built = Some(built);
            self.command_list.query.clear();
            self.set_command_list_items(rows.items);
        }
        self.command_list.set_query(filter);
    }

    /// Show the rows again after the library changed, keeping the selected
    /// row.
    fn reshow_bookmarks(&mut self) {
        let selected = self
            .command_list
            .item(self.selected_index)
            .map(|item| item.identity.clone());
        self.bookmarks.built = None;
        self.show_bookmarks();
        self.selected_index = selected
            .and_then(|identity| self.command_list.position(&identity))
            .unwrap_or(0);
    }

    pub(super) fn handle_bookmarks_loaded(
        &mut self,
        generation: u64,
        library: Option<Arc<Library>>,
    ) -> Task<Message> {
        if !self.command_list.is_current(generation) {
            return Task::none();
        }
        let Some(library) = library else {
            self.command_list
                .set_notice("Loading bookmarks stopped unexpectedly".to_string());
            return Task::none();
        };
        if self
            .bookmarks
            .view
            .is_some_and(|view| view >= library.sources.len())
        {
            self.bookmarks.view = None;
        }
        self.index_bookmark_searches(&library);
        self.bookmarks.library = Some(library.clone());
        self.reshow_bookmarks();
        let icons = self.lookup_bookmark_icons(library, true);
        // A keyword typed before the bookmarks loaded was not known yet.
        match self
            .bookmarks
            .prompt
            .is_none()
            .then(|| self.open_bookmark_keyword())
            .flatten()
        {
            Some(prompt) => Task::batch([prompt, icons]),
            None => icons,
        }
    }

    fn lookup_bookmark_icons(&self, library: Arc<Library>, fetch: bool) -> Task<Message> {
        let session = self.bookmarks.session;
        Task::perform(
            off_thread(move || bookmarks::lookup_icons(&library)),
            move |icons| Message::BookmarkIconsLoaded(session, icons.unwrap_or_default(), fetch),
        )
    }

    /// Show the site icons found, then download the missing ones once.
    pub(super) fn handle_bookmark_icons_loaded(
        &mut self,
        session: u64,
        icons: HashMap<String, PathBuf>,
        fetch: bool,
    ) -> Task<Message> {
        if !self.bookmarks_session_is(session) {
            return Task::none();
        }
        if let Some(library) = &mut self.bookmarks.library {
            if !icons.is_empty() && library.icons != icons {
                Arc::make_mut(library).icons = icons;
                let library = library.clone();
                self.index_bookmark_searches(&library);
                self.reshow_bookmarks();
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
    pub(super) fn handle_bookmark_favicons_fetched(&mut self, session: u64) -> Task<Message> {
        match &self.bookmarks.library {
            Some(library) if self.bookmarks_session_is(session) => {
                self.lookup_bookmark_icons(library.clone(), false)
            }
            _ => Task::none(),
        }
    }

    fn bookmarks_session_is(&self, session: u64) -> bool {
        self.command_list.addon == Some(CommandListAddon::Bookmarks)
            && self.command_list.session == session
            && self.bookmarks.session == session
    }

    /// Enter or Tab on a folder row: show that folder.
    pub(super) fn open_bookmark_folder(&mut self, row: usize) -> Option<Task<Message>> {
        if self.command_list.addon != Some(CommandListAddon::Bookmarks) {
            return None;
        }
        let identity = &self.command_list.item(row)?.identity;
        let path = self.bookmarks.folder_path(identity)?.to_string();
        Some(self.fill_bookmarks_query(&path))
    }

    fn fill_bookmarks_query(&mut self, text: &str) -> Task<Message> {
        let query = format!("{} {text}", self.addons.bookmarks.trigger());
        self.search_query = query.clone();
        let input_id = self.view.search_input_id.clone();
        self.update(Message::SearchChanged(query))
            .chain(move_cursor_to_end(input_id))
    }

    /// Backspace on the `/` ending the folder path: the query of the parent
    /// folder and the path of the folder left, as the file browser goes up.
    pub(super) fn bookmarks_updir(&self, query: &str) -> Option<(String, String)> {
        if self.command_list.addon != Some(CommandListAddon::Bookmarks)
            || self.search_query.strip_suffix('/') != Some(query)
        {
            return None;
        }
        let left = self.bookmarks.query.as_str();
        let name = left.strip_suffix('/')?.rsplit('/').next()?;
        let parent = query.strip_suffix(name)?;
        Some((parent.to_string(), left.to_string()))
    }

    /// Select the folder row `path` opens, after going up from it.
    pub(super) fn reselect_bookmark_folder(&mut self, path: &str) {
        let path = path.to_lowercase();
        let Some(position) = self
            .bookmarks
            .folders
            .iter()
            .find(|(_, folder)| folder.to_lowercase() == path)
            .and_then(|(identity, _)| self.command_list.position(identity))
        else {
            return;
        };
        self.selected_index = self.command_list_offset() + position;
    }

    /// Ctrl+Tab: show every source, then each one alone. The folder path is
    /// dropped since it names the source when several show.
    pub(super) fn handle_bookmarks_cycle_view(&mut self, backwards: bool) -> Task<Message> {
        if self.bookmarks.library.is_none() {
            return Task::none();
        }
        self.bookmarks.view = self.bookmarks.next_view(backwards);
        self.bookmarks.built = None;
        self.selected_index = 0;
        let (path, filter) = split_query(&self.bookmarks.query);
        if !path.is_empty() {
            let filter = filter.to_string();
            return self.fill_bookmarks_query(&filter);
        }
        self.show_bookmarks();
        Task::none()
    }

    fn index_bookmark_searches(&self, library: &Library) {
        if let Ok(mut index) = self.bookmark_searches.lock() {
            *index = SearchIndex::new(library);
        }
    }

    fn bookmark_search(&self, identity: &str) -> Option<SearchTarget> {
        self.bookmark_searches.lock().ok()?.get(identity).cloned()
    }

    fn bookmark_keyword(&self, keyword: &str) -> Option<SearchTarget> {
        self.bookmark_searches
            .lock()
            .ok()?
            .keyword(keyword)
            .cloned()
    }

    /// `bm <keyword> `: ask for the term of the keyword's search bookmark.
    /// The trimmed list query cannot tell whether a space follows the
    /// keyword, so this looks at the typed text.
    fn open_bookmark_keyword(&mut self) -> Option<Task<Message>> {
        let trigger = self.addons.bookmarks.trigger().to_string();
        let typed = self
            .search_query
            .strip_prefix(&trigger)?
            .strip_prefix(' ')?;
        let (keyword, term) = keyword_query(typed)?;
        let target = self.bookmark_keyword(keyword)?;
        let (back, term) = (format!("{trigger} {keyword}"), term.to_string());
        Some(self.enter_search_prompt(target, &term, back))
    }

    /// `<keyword> ` in the main view, when it lists bookmarks.
    pub(super) fn open_main_view_bookmark_keyword(&mut self, query: &str) -> Option<Task<Message>> {
        if !self.main_view_lists(CommandListAddon::Bookmarks) {
            return None;
        }
        let (keyword, term) = keyword_query(query)?;
        let target = self.bookmark_keyword(keyword)?;
        let (back, term) = (keyword.to_string(), term.to_string());
        Some(self.enter_search_prompt(target, &term, back))
    }

    /// Enter or Tab on a search bookmark row: ask for the term. Ctrl+Enter
    /// still copies the link.
    pub(super) fn open_bookmark_search(&mut self, row: usize) -> Option<Task<Message>> {
        if self.command_list.addon != Some(CommandListAddon::Bookmarks)
            || self.bookmarks.prompt.is_some()
            || self.view.current_modifiers.control()
        {
            return None;
        }
        let identity = self.command_list.item(row)?.identity.clone();
        let target = self.bookmark_search(&identity)?;
        let back = self.search_query.clone();
        Some(self.enter_search_prompt(target, "", back))
    }

    /// Enter on a search bookmark of the main view.
    pub(super) fn open_main_view_bookmark_search(
        &mut self,
        identity: &str,
    ) -> Option<Task<Message>> {
        if self.view.current_modifiers.control() {
            return None;
        }
        let target = self.bookmark_search(identity)?;
        let back = self.search_query.clone();
        Some(self.enter_search_prompt(target, "", back))
    }

    /// Show the term prompt for `target`; leaving it shows `back` again.
    fn enter_search_prompt(
        &mut self,
        target: SearchTarget,
        term: &str,
        back: String,
    ) -> Task<Message> {
        let mut tasks = Vec::new();
        if self.command_list.addon != Some(CommandListAddon::Bookmarks) {
            let trigger = self.addons.bookmarks.trigger();
            tasks.push(self.handle_search_changed(format!("{trigger} ")));
        }
        crate::debug_log!("bookmarks: searching with {}", target.identity);
        self.bookmarks.prompt = Some(SearchPrompt::new(target, term, back));
        self.show_search_prompt();
        let input = self.view.search_input_id.clone();
        tasks.push(focus(input.clone()).chain(move_cursor_to_end(input)));
        Task::batch(tasks)
    }

    /// Show the row of the term prompt, when one is open.
    fn show_search_prompt(&mut self) -> bool {
        let Some(prompt) = &self.bookmarks.prompt else {
            return false;
        };
        let items = prompt.items(self.bookmark_options());
        self.bookmarks.built = None;
        self.command_list.query.clear();
        self.set_command_list_items(items);
        self.selected_index = self.command_list_offset();
        true
    }

    /// Typing in the term prompt; Backspace with no term leaves it.
    pub(super) fn handle_search_prompt_input(&mut self, term: String) -> Task<Message> {
        let Some(prompt) = self.bookmarks.prompt.as_mut() else {
            return Task::none();
        };
        if term.is_empty() && prompt.term.is_empty() {
            return self.leave_search_prompt();
        }
        prompt.term = term;
        self.show_search_prompt();
        Task::none()
    }

    /// Close the term prompt and show the query it was opened from, with
    /// the bookmark selected.
    pub(super) fn leave_search_prompt(&mut self) -> Task<Message> {
        let Some(prompt) = self.bookmarks.prompt.take() else {
            return Task::none();
        };
        let task = self.handle_search_changed(prompt.back);
        if let Some(position) = self
            .command_list
            .position(&prompt.target.identity)
            .filter(|_| self.command_list.addon == Some(CommandListAddon::Bookmarks))
        {
            self.selected_index = self.command_list_offset() + position;
        }
        // Escape took the focus from the search field.
        let input = self.view.search_input_id.clone();
        Task::batch([task, focus(input.clone()).chain(move_cursor_to_end(input))])
    }
}
