use iced::widget::operation::{focus, move_cursor_to_end};
use iced::Task;

use super::actions::DeferredAction;
use super::app::QueryMode;
use super::command_list::{CommandListAddon, CommandListState};
use super::main_view::{Focus, Source};
use super::state::LauncherApp;
use super::types::Message;

impl LauncherApp {
    pub(super) fn cleared_filter_query(&self) -> Option<String> {
        let query = match &self.query_class.mode {
            QueryMode::Standard => return (!self.search_query.is_empty()).then(String::new),
            QueryMode::FileBrowser { filter, .. } => filter,
            QueryMode::CommandList { .. } => &self.command_list.query,
            QueryMode::ScriptFilter { query, .. }
            | QueryMode::TextSnippet { query, .. }
            | QueryMode::Emoji { query }
            | QueryMode::Clipboard { query }
            | QueryMode::WebSearch { query, .. } => query,
            QueryMode::Devtools { payload } => payload,
        };
        if query.is_empty() {
            return None;
        }
        self.search_query
            .trim_end()
            .strip_suffix(query)
            .map(str::to_string)
    }

    pub(super) fn clear_filter(&mut self) -> Task<Message> {
        let Some(query) = self.cleared_filter_query() else {
            return Task::none();
        };
        self.update(Message::SearchChanged(query))
            .chain(focus(self.view.search_input_id.clone()))
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn command_status_recovery(&self) -> Option<(&'static str, Message)> {
        let list = &self.command_list;
        if list.status().is_none() || list.loading || list.overlay.is_some() || self.vault.busy() {
            return None;
        }
        if list.unavailable {
            Some(("Open setup help", Message::OpenAddonSetup))
        } else {
            Some(("Retry · Ctrl+R", Message::RetryLoad))
        }
    }

    pub(super) fn clipboard_status_recovery(&self) -> Option<(&'static str, Message)> {
        if self.clipboard.loading {
            return None;
        }
        if self.cleared_filter_query().is_some() && self.clipboard.error.is_none() {
            Some(("Clear filter", Message::ClearFilter))
        } else {
            Some(("Retry · Ctrl+R", Message::RetryLoad))
        }
    }

    fn sources_loading(&self) -> bool {
        self.main_view.active
            && self
                .main_view
                .sources
                .iter()
                .enumerate()
                .any(|(index, source)| {
                    self.main_view.shows(index)
                        && match source {
                            Source::List(list) => list.loading,
                            Source::Snippets { loading, .. } => *loading,
                            Source::ScriptFilter { running, .. } => *running,
                        }
                })
    }

    pub(super) fn empty_state_message(&self) -> String {
        if self.sources_loading() {
            return "Searching sources…".into();
        }
        if self.emoji.active && self.emoji.data_loading {
            return "Loading emoji…".into();
        }
        if self.file_browser.active {
            return if self.file_browser.all_entries.is_empty()
                && self.cleared_filter_query().is_none()
            {
                "This directory is empty.".into()
            } else {
                "No files match this filter or view.".into()
            };
        }
        if let Some(addon) = self.command_list.addon {
            return if self.command_list.query.is_empty() {
                format!("No {} results available.", addon.badge().to_lowercase())
            } else {
                format!(
                    "No {} results match your search.",
                    addon.badge().to_lowercase()
                )
            };
        }
        "No results match your search.".into()
    }

    pub(super) fn empty_state_recovery(&self) -> Option<(&'static str, Message)> {
        if self.sources_loading() {
            return None;
        }
        if self.emoji.active && self.emoji.data_loading {
            return None;
        }
        if self.file_browser.error.is_some() || self.text_snippets.error.is_some() {
            return Some(("Retry · Ctrl+R", Message::RetryLoad));
        }
        if self.cleared_filter_query().is_some() {
            return Some(("Clear filter", Message::ClearFilter));
        }
        if self.file_browser.active && !self.file_browser.all_entries.is_empty() {
            return Some(("Next file view · Ctrl+Tab", Message::CycleView(false)));
        }
        if self.file_browser.active
            || self.main_view.focus.is_some()
            || self.text_snippets.active
            || self.command_list.is_active()
        {
            Some(("Retry · Ctrl+R", Message::RetryLoad))
        } else {
            Some(("Browse keywords · Ctrl+/", Message::ToggleHelp))
        }
    }

    pub(super) fn visible_command_list(&self) -> Option<&CommandListState> {
        if self.command_list.is_active() {
            return Some(&self.command_list);
        }
        let source = match self.main_view.focus {
            Some(Focus::Source(source)) => source,
            _ => self.selected_main_view()?.0,
        };
        match self.main_view.sources.get(source)? {
            Source::List(list) => Some(list),
            _ => None,
        }
    }

    pub(super) fn open_addon_setup(&mut self) -> Task<Message> {
        let Some(addon) = self.command_list.addon else {
            return Task::none();
        };
        let page = match addon {
            CommandListAddon::System => "system-commands",
            CommandListAddon::Windows => "window-switcher",
            CommandListAddon::BrowserTabs => "browser-tabs",
            CommandListAddon::Bookmarks => "bookmarks",
            CommandListAddon::BrowserHistory => "browser-history",
            CommandListAddon::Media => "media-controls",
            CommandListAddon::Audio => "audio-switcher",
            CommandListAddon::Kill => "kill-process",
            CommandListAddon::Bluetooth => "bluetooth",
            CommandListAddon::Pass => "password-store",
            CommandListAddon::Bitwarden => "bitwarden",
            CommandListAddon::Terminal => "terminal",
            CommandListAddon::Slack => "slack",
        };
        if let Ok(mut action) = self.deferred_action.lock() {
            *action = Some(DeferredAction::Command(vec![
                "xdg-open".into(),
                format!("https://chmouel.github.io/raffi/features/{page}/"),
            ]));
        }
        iced::exit()
    }
}
