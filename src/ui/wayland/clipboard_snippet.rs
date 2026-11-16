//! Saving a clipboard entry as a snippet in a `directory` or `markdown` source.
//! The browser's action menu offers one save per such source; the entry's text
//! then takes a name and an optional keyword in the search field before it is
//! written.

use iced::widget::operation::{focus, move_cursor_to_end};
use iced::Task;

use super::clipboard::{History, ItemKind};
use super::snippets::{save_snippet, snippet_save_path};
use super::state::LauncherApp;
use super::types::Message;
use crate::TextSnippet;

/// Longest name suggested from the first line of an entry.
const SUGGESTED_NAME_CHARS: usize = 60;

/// A clipboard text waiting for its name, then its keyword.
#[derive(Debug)]
pub(super) struct SnippetSave {
    /// Config index of the snippet source to write to.
    pub source: usize,
    /// The clipboard browser session the prompt was opened in.
    pub session: u64,
    pub value: String,
    /// The name once it is entered; `None` while the name is asked for.
    pub name: Option<String>,
    /// What the search field holds for the current step.
    pub buffer: String,
}

impl SnippetSave {
    pub fn placeholder(&self) -> &'static str {
        match self.name {
            None => "Snippet name…",
            Some(_) => "Keyword (optional, Enter to skip)…",
        }
    }
}

/// The name suggested for `value`: its first non-blank line, shortened.
pub(super) fn suggested_name(value: &str) -> String {
    let line = value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(SUGGESTED_NAME_CHARS) {
        Some((end, _)) => line[..end].trim_end().to_string(),
        None => line.to_string(),
    }
}

impl LauncherApp {
    /// Whether the save prompt is up in the clipboard browser.
    pub(super) fn snippet_save_open(&self) -> bool {
        self.clipboard.active
            && self
                .snippet_save
                .as_ref()
                .is_some_and(|save| save.session == self.clipboard.session)
    }

    /// Start saving the selected clipboard entry to the snippet source at
    /// config index `source`. Fetches the full text first, since the list shows
    /// an excerpt.
    pub(super) fn handle_save_clipboard_snippet(&mut self, source: usize) -> Task<Message> {
        let state = &self.clipboard;
        if !state.active || state.pending {
            return Task::none();
        }
        let Some(item) = self
            .selected_clipboard_item()
            .and_then(|index| state.items.get(index))
        else {
            return Task::none();
        };
        if item.kind != ItemKind::Text {
            self.clipboard.error = Some("Only plain text can be saved as a snippet".into());
            return Task::none();
        }
        let reference = item.reference(&state.tab);
        let session = state.session;
        let history = History::new(self.addons.clipboard.backend);
        self.clipboard.pending = true;
        self.clipboard.error = None;
        Task::perform(
            async move { history.snippet_text(&reference) },
            move |result| Message::ClipboardSnippetText(session, source, result),
        )
    }

    /// The full text arrived: open the name prompt in the search field.
    pub(super) fn handle_clipboard_snippet_text(
        &mut self,
        session: u64,
        source: usize,
        result: Result<String, String>,
    ) -> Task<Message> {
        let state = &mut self.clipboard;
        if !state.active || session != state.session || !state.pending {
            return Task::none();
        }
        state.pending = false;
        match result {
            Ok(value) => {
                self.snippet_save = Some(SnippetSave {
                    source,
                    session,
                    buffer: suggested_name(&value),
                    value,
                    name: None,
                });
                self.clipboard.error = None;
                focus(self.view.search_input_id.clone())
                    .chain(move_cursor_to_end(self.view.search_input_id.clone()))
            }
            Err(error) => {
                self.clipboard.error = Some(error);
                Task::none()
            }
        }
    }

    /// Typing in the save prompt.
    pub(super) fn handle_snippet_save_input(&mut self, value: String) -> Task<Message> {
        if let Some(save) = self.snippet_save.as_mut() {
            save.buffer = value;
        }
        self.clipboard.error = None;
        Task::none()
    }

    /// Enter in the save prompt: the name first, then the keyword and the
    /// write. A failed write keeps the prompt open with the reason shown.
    pub(super) fn submit_snippet_save(&mut self) -> Task<Message> {
        let Some(save) = self.snippet_save.as_mut() else {
            return Task::none();
        };
        let Some(name) = save.name.clone() else {
            let name = save.buffer.trim().to_string();
            if name.is_empty() {
                self.clipboard.error = Some("Give the snippet a name".into());
                return Task::none();
            }
            save.name = Some(name);
            save.buffer.clear();
            return Task::none();
        };
        let keyword = save.buffer.trim();
        let snippet = TextSnippet {
            name,
            value: save.value.clone(),
            icon: None,
            keyword: (!keyword.is_empty()).then(|| keyword.to_string()),
        };
        let Some(config) = self.addons.text_snippets.get(save.source) else {
            self.snippet_save = None;
            self.clipboard.error = Some("The snippet source is no longer configured".into());
            return Task::none();
        };
        let path = snippet_save_path(config).map(str::to_string);
        match save_snippet(config, &snippet) {
            Ok(()) => {
                if let Some(path) = path {
                    self.text_snippets.file_cache.remove(&path);
                }
                self.snippet_save = None;
                self.clipboard.error = None;
                focus(self.view.search_input_id.clone())
                    .chain(move_cursor_to_end(self.view.search_input_id.clone()))
            }
            Err(error) => {
                self.clipboard.error = Some(error);
                Task::none()
            }
        }
    }

    /// Escape in the save prompt: drop it and return to the list.
    pub(super) fn cancel_snippet_save(&mut self) -> Task<Message> {
        self.snippet_save = None;
        self.clipboard.error = None;
        focus(self.view.search_input_id.clone())
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::suggested_name;

    #[test]
    fn test_suggested_name_takes_the_first_non_blank_line() {
        assert_eq!(suggested_name("\n  \n  Hello there\nmore"), "Hello there");
        assert_eq!(suggested_name(""), "");
    }

    #[test]
    fn test_suggested_name_is_shortened_on_a_char_boundary() {
        let long = "é".repeat(100);
        assert_eq!(suggested_name(&long).chars().count(), 60);
    }
}
