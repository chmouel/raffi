use std::collections::HashSet;

use iced::Task;

use super::clipboard::{CopyQ, Page};
use super::state::{LauncherApp, Message};
use crate::ClipboardAction;

/// Scans restarted because the history changed before one is given up.
const MAX_SCAN_RESTARTS: u32 = 3;
/// Previews kept for quick back-and-forth navigation.
const PREVIEW_CACHE_SIZE: usize = 8;

impl LauncherApp {
    /// Enter or update the clipboard browser for `query`.
    pub(super) fn handle_clipboard_query(&mut self, query: String, tasks: &mut Vec<Task<Message>>) {
        let state = &mut self.clipboard;
        if state.active && state.query == query {
            let follow = state.follow.clone();
            self.restore_clipboard_selection(follow.as_deref());
            return;
        }
        if !state.active {
            state.clear();
            state.active = true;
        }
        state.query = query;
        state.follow = None;
        state.restarts = 0;
        tasks.push(self.start_clipboard_scan());
    }

    /// Start listing the history from the first row. Items already listed
    /// stay visible until the first page arrives.
    fn start_clipboard_scan(&mut self) -> Task<Message> {
        let state = &mut self.clipboard;
        state.generation = state.generation.wrapping_add(1);
        state.loading = true;
        state.fresh = true;
        state.scanned = 0;
        let generation = state.generation;
        let query = state.query.clone();
        let kind = state.view.kind();
        let min_chars = self.addons.clipboard.min_chars();
        Task::perform(
            async move { CopyQ::default().list_page("", 0, &query, kind, min_chars) },
            move |result| Message::ClipboardPage(generation, result),
        )
    }

    pub(super) fn handle_clipboard_page(
        &mut self,
        generation: u64,
        result: Result<Page, String>,
    ) -> Task<Message> {
        let state = &mut self.clipboard;
        if !state.active || generation != state.generation {
            return Task::none();
        }
        let page = match result {
            Ok(page) => page,
            Err(error) => {
                if state.fresh {
                    state.items.clear();
                    state.fresh = false;
                }
                state.loading = false;
                state.error = Some(error);
                self.clamp_clipboard_selection();
                return Task::none();
            }
        };

        let changed = !state.fresh
            && (page.tab != state.tab || page.head != state.head || page.total != state.total);
        if changed || !page.stable {
            state.restarts += 1;
            if state.restarts > MAX_SCAN_RESTARTS {
                if state.fresh {
                    state.items.clear();
                    state.fresh = false;
                }
                state.loading = false;
                state.error = Some("Clipboard history kept changing; press Ctrl+R to retry".into());
                self.clamp_clipboard_selection();
                return Task::none();
            }
            crate::debug_log!("clipboard: history changed during scan, restarting");
            return self.start_clipboard_scan();
        }

        if state.fresh {
            state.fresh = false;
            state.error = None;
            state.tab = page.tab;
            state.head = page.head;
            state.total = page.total;
            state.items.clear();
        }
        state.scanned = page.next;
        state.items.extend(page.items);
        state.items.sort_by_key(|item| (item.tier, item.row));
        let mut seen = HashSet::new();
        state.items.retain(|item| seen.insert(item.id.clone()));

        let follow = state.follow.clone();
        self.restore_clipboard_selection(follow.as_deref());

        let state = &mut self.clipboard;
        if page.next < state.total {
            let generation = state.generation;
            let tab = state.tab.clone();
            let query = state.query.clone();
            let kind = state.view.kind();
            let min_chars = self.addons.clipboard.min_chars();
            let start = page.next;
            Task::perform(
                async move { CopyQ::default().list_page(&tab, start, &query, kind, min_chars) },
                move |result| Message::ClipboardPage(generation, result),
            )
        } else {
            state.loading = false;
            Task::none()
        }
    }

    /// Display index of the first clipboard row.
    pub(super) fn clipboard_offset(&self) -> usize {
        let mut offset = self.keyword_rows();
        if self.script_filter.loading {
            offset += 1;
        } else if let Some(result) = &self.script_filter.results {
            offset += result.items.len();
        }
        if self.text_snippets.loading {
            offset += 1;
        } else if self.text_snippets.active {
            offset += self.text_snippets.filtered.len();
        }
        if self.emoji.active {
            offset += self.emoji.filtered.len();
        }
        offset
    }

    /// Index into `clipboard.items` of the selected row, if it is an item.
    pub(super) fn selected_clipboard_item(&self) -> Option<usize> {
        if !self.clipboard.active || self.view.help_active {
            return None;
        }
        let index = self.selected_index.checked_sub(self.clipboard_offset())?;
        (index < self.clipboard.items.len()).then_some(index)
    }

    fn selected_clipboard_id(&self) -> Option<String> {
        self.selected_clipboard_item()
            .map(|index| self.clipboard.items[index].id.clone())
    }

    /// Remember the selected item so it stays selected when results move.
    pub(super) fn follow_clipboard_selection(&mut self) {
        if self.clipboard.active {
            self.clipboard.follow = self.selected_clipboard_id();
        }
    }

    fn restore_clipboard_selection(&mut self, follow: Option<&str>) {
        if let Some(id) = follow {
            if let Some(position) = self.clipboard.items.iter().position(|item| item.id == id) {
                self.selected_index = self.clipboard_offset() + position;
                return;
            }
        }
        self.clamp_clipboard_selection();
    }

    fn clamp_clipboard_selection(&mut self) {
        let total = self.total_items();
        if self.selected_index >= total {
            self.selected_index = total.saturating_sub(1);
        }
    }

    pub(super) fn handle_clipboard_refresh(&mut self) -> Task<Message> {
        if !self.clipboard.active || self.view.help_active {
            return Task::none();
        }
        self.follow_clipboard_selection();
        let state = &mut self.clipboard;
        state.restarts = 0;
        state.error = None;
        // Ids name contents, so cached previews stay valid; failures may not.
        state.previews.retain(|(_, preview)| preview.is_ok());
        self.start_clipboard_scan()
    }

    /// Ctrl+Tab: list the next view, all items, images or URLs.
    pub(super) fn handle_clipboard_cycle_view(&mut self, backwards: bool) -> Task<Message> {
        if !self.clipboard.active || self.view.help_active {
            return Task::none();
        }
        self.follow_clipboard_selection();
        let state = &mut self.clipboard;
        state.view = state.view.cycle(backwards);
        state.restarts = 0;
        state.error = None;
        self.start_clipboard_scan()
    }

    /// Request the preview of the selected item when it is not cached and no
    /// other preview is loading. Called after every update.
    pub(super) fn ensure_clipboard_preview(&mut self) -> Task<Message> {
        if !self.view.show_preview || self.clipboard.preview_inflight.is_some() {
            return Task::none();
        }
        let Some(index) = self.selected_clipboard_item() else {
            return Task::none();
        };
        let state = &mut self.clipboard;
        let item = &state.items[index];
        if state.preview(&item.id).is_some() {
            return Task::none();
        }
        let id = item.id.clone();
        let reference = item.reference(&state.tab);
        let session = state.session;
        state.preview_inflight = Some(id.clone());
        Task::perform(
            async move { CopyQ::default().preview(&reference) },
            move |result| Message::ClipboardPreview(session, id.clone(), result),
        )
    }

    pub(super) fn handle_clipboard_preview(
        &mut self,
        session: u64,
        id: String,
        result: Result<super::clipboard::ClipPreview, String>,
    ) -> Task<Message> {
        let state = &mut self.clipboard;
        if !state.active || session != state.session {
            return Task::none();
        }
        if state.preview_inflight.as_deref() == Some(id.as_str()) {
            state.preview_inflight = None;
        }
        state.previews.retain(|(cached, _)| *cached != id);
        if state.previews.len() >= PREVIEW_CACHE_SIZE {
            state.previews.drain(..1);
        }
        state.previews.push((id, result));
        Task::none()
    }

    pub(super) fn handle_clipboard_selected(&mut self, index: usize) -> Task<Message> {
        let state = &self.clipboard;
        if state.pending {
            return Task::none();
        }
        let Some(item) = state.items.get(index) else {
            return Task::none();
        };
        let reference = item.reference(&state.tab);
        let config = &self.addons.clipboard;
        let action = if self.view.current_modifiers.control() {
            config.secondary_action
        } else {
            config.action
        };
        let session = state.session;

        let task = match action {
            ClipboardAction::Copy => Task::perform(
                async move { CopyQ::default().select(&reference).map(|()| None) },
                move |result| Message::ClipboardActionDone(session, result),
            ),
            ClipboardAction::HyprPaste => {
                let target = match self.hyprland.clone() {
                    Some(Ok(target)) => target,
                    Some(Err(error)) => {
                        self.clipboard.error = Some(format!("Cannot paste: {error}"));
                        return Task::none();
                    }
                    None => {
                        self.clipboard.error = Some(
                            "Pasting needs a Hyprland session; use the copy action instead".into(),
                        );
                        return Task::none();
                    }
                };
                Task::perform(
                    async move {
                        target.check_target()?;
                        CopyQ::default().select(&reference)?;
                        Ok(Some(target))
                    },
                    move |result| Message::ClipboardActionDone(session, result),
                )
            }
        };
        self.clipboard.pending = true;
        self.clipboard.error = None;
        task
    }

    pub(super) fn handle_clipboard_action_done(
        &mut self,
        session: u64,
        result: Result<Option<super::hyprland::Hyprland>, String>,
    ) -> Task<Message> {
        let state = &mut self.clipboard;
        if !state.active || session != state.session || !state.pending {
            return Task::none();
        }
        state.pending = false;
        let is_hypr_paste = matches!(&result, Ok(Some(_)));
        let outcome = result.and_then(|target| {
            if let Some(target) = target {
                self.deferred_action
                    .lock()
                    .map_err(|_| "cannot queue Hyprland paste".to_string())?
                    .replace(super::actions::DeferredAction::HyprPaste(target));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => {
                if is_hypr_paste || self.should_close(self.addons.clipboard.close_on_action) {
                    iced::exit()
                } else {
                    Task::none()
                }
            }
            Err(error) => {
                crate::debug_log!("clipboard: action failed");
                self.clipboard.error = Some(error);
                Task::none()
            }
        }
    }

    pub(super) fn handle_clipboard_delete(&mut self) -> Task<Message> {
        if !self.clipboard.active || self.view.help_active {
            return Task::none();
        }
        let state = &self.clipboard;
        if state.pending {
            return Task::none();
        }
        let Some(index) = self.selected_clipboard_item() else {
            return Task::none();
        };
        let item = &state.items[index];
        let reference = item.reference(&state.tab);
        let id = item.id.clone();
        let session = state.session;
        self.clipboard.pending = true;
        self.clipboard.error = None;
        Task::perform(
            async move { CopyQ::default().delete(&reference) },
            move |result| Message::ClipboardDeleteDone(session, result.map(|()| id.clone())),
        )
    }

    pub(super) fn handle_clipboard_delete_done(
        &mut self,
        session: u64,
        result: Result<String, String>,
    ) -> Task<Message> {
        let state = &mut self.clipboard;
        if !state.active || session != state.session || !state.pending {
            return Task::none();
        }
        state.pending = false;
        match result {
            Ok(id) => {
                state.items.retain(|item| item.id != id);
                state.previews.retain(|(cached, _)| *cached != id);
                if state.preview_inflight.as_deref() == Some(id.as_str()) {
                    state.preview_inflight = None;
                }
                state.total = state.total.saturating_sub(1);
                self.clamp_clipboard_selection();
            }
            Err(error) => {
                self.clipboard.error = Some(error);
            }
        }
        Task::none()
    }
}
