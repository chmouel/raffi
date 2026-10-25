use iced::keyboard::Modifiers;
use iced::widget::operation::{focus, move_cursor_to_end};
use iced::Task;

use super::main_view::{MainRowRef, Source};
use super::state::{LauncherApp, MainRowKey};
use super::types::Message;
use super::vault::{EntryAction, Field, VaultAddon};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Main(MainRowKey),
    Command(u64, String),
    Other(String, usize, String),
}

#[derive(Debug, Clone)]
pub(super) struct ActionEntry {
    pub label: String,
    pub shortcut: String,
    message: Message,
    modifiers: Modifiers,
}

pub(super) struct ActionMenu {
    pub title: String,
    pub query: String,
    pub selected: usize,
    pub entries: Vec<ActionEntry>,
    target: Target,
}

impl ActionMenu {
    pub fn filtered(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                query
                    .split_whitespace()
                    .all(|word| entry.label.to_lowercase().contains(word))
                    .then_some(index)
            })
            .collect()
    }
}

impl LauncherApp {
    fn action_target(&self) -> Option<Target> {
        if self.view.help_active
            || self.vault.prompt.is_some()
            || self.bookmarks.prompt.is_some()
            || self.selected_index < self.keyword_rows()
        {
            return None;
        }
        if let Some(row) = self.selected_main_row() {
            if let MainRowRef::Source { source, row } = row {
                if let Some(Source::List(list)) = self.main_view.sources.get(source) {
                    if list.item(row)?.unavailable.is_some() {
                        return None;
                    }
                }
            }
            return self.main_row_key(row).map(Target::Main);
        }
        if self.command_list.is_active() {
            let row = self
                .selected_index
                .checked_sub(self.command_list_offset())?;
            let item = self.command_list.item(row)?;
            return item
                .unavailable
                .is_none()
                .then(|| Target::Command(self.command_list.session, item.identity.clone()));
        }
        let preview = self.selected_preview_data(true)?;
        Some(Target::Other(
            self.search_query.clone(),
            self.selected_index,
            preview.title,
        ))
    }

    pub(super) fn selected_actions(&self) -> Vec<ActionEntry> {
        if self.action_target().is_none() {
            return Vec::new();
        }
        let Some(preview) = self.selected_preview_data(true) else {
            return Vec::new();
        };
        let vault = self.command_list.addon.and_then(VaultAddon::of).is_some();
        let list_row = self
            .selected_main_view()
            .and_then(|(source, row)| match self.main_view.sources.get(source)? {
                Source::List(list) => Some((list.as_ref(), row)),
                _ => None,
            })
            .or_else(|| {
                self.selected_index
                    .checked_sub(self.command_list_offset())
                    .map(|row| (&self.command_list, row))
            });
        let mut metadata = preview.metadata;
        if let Some((list, row)) = list_row {
            if let Some(addon) = list.addon.and_then(VaultAddon::of) {
                if let Some(item) = list.item(row) {
                    let entry = match addon {
                        VaultAddon::Pass => super::pass::entry_name(&item.identity),
                        VaultAddon::Bitwarden => super::bitwarden::item_id(&item.identity),
                    };
                    if entry.is_some() {
                        metadata.extend([
                            ("Enter".into(), "Copy the password".into()),
                            ("Ctrl+Enter".into(), "Type the password".into()),
                        ]);
                        if vault {
                            metadata.extend([
                                ("Ctrl+U".into(), "Copy the username".into()),
                                ("Ctrl+O".into(), "Copy the OTP code".into()),
                            ]);
                        }
                    }
                }
            }
        }
        metadata
            .into_iter()
            .filter_map(|(shortcut, label)| {
                let (message, modifiers) = match shortcut.as_str() {
                    "Enter" => (Message::Submit, Modifiers::empty()),
                    "Ctrl+Enter" => (Message::Submit, Modifiers::CTRL),
                    "Ctrl+D" if self.command_list.is_active() || self.clipboard.active => {
                        (Message::DeleteSelected, Modifiers::empty())
                    }
                    "Ctrl+U" if vault => (
                        Message::VaultShortcut(EntryAction::Copy(Field::Username)),
                        Modifiers::empty(),
                    ),
                    "Ctrl+O" if vault || self.file_browser.active => {
                        (Message::FileBrowserOpenSelected, Modifiers::empty())
                    }
                    "Ctrl+B" if self.file_browser.active => {
                        (Message::FileBrowserOpenFileManager, Modifiers::empty())
                    }
                    "Ctrl+Y" if self.file_browser.active => {
                        (Message::FileBrowserCopyPath, Modifiers::empty())
                    }
                    "Ctrl+Alt+Y" if self.file_browser.active => {
                        (Message::FileBrowserCopyImage, Modifiers::empty())
                    }
                    _ => return None,
                };
                Some(ActionEntry {
                    label,
                    shortcut,
                    message,
                    modifiers,
                })
            })
            .collect()
    }

    pub(super) fn open_action_menu(&mut self) -> Task<Message> {
        let entries = self.selected_actions();
        let Some(target) = self.action_target().filter(|_| !entries.is_empty()) else {
            return Task::none();
        };
        let title = self
            .selected_preview_data(true)
            .map(|preview| preview.title)
            .unwrap_or_default();
        self.main_view.selection_deliberate = true;
        self.action_menu = Some(ActionMenu {
            title,
            query: String::new(),
            selected: 0,
            entries,
            target,
        });
        focus(self.view.search_input_id.clone())
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn close_action_menu(&mut self) -> Task<Message> {
        self.action_menu = None;
        focus(self.view.search_input_id.clone())
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn execute_menu_action(&mut self, index: usize) -> Task<Message> {
        let Some(menu) = self.action_menu.take() else {
            return Task::none();
        };
        if self.action_target().as_ref() != Some(&menu.target) {
            return self.close_action_menu();
        }
        let Some(entry) = menu.entries.get(index) else {
            return self.close_action_menu();
        };
        let modifiers = self.view.current_modifiers;
        self.view.current_modifiers = entry.modifiers;
        let task = self.update(entry.message.clone());
        self.view.current_modifiers = modifiers;
        task.chain(focus(self.view.search_input_id.clone()))
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn update_action_menu(&mut self, message: &Message) -> Option<Task<Message>> {
        let menu = self.action_menu.as_mut()?;
        match message {
            Message::SearchChanged(query) => {
                menu.query.clone_from(query);
                menu.selected = 0;
            }
            Message::MoveUp | Message::MoveDown => {
                let count = menu.filtered().len();
                if count > 0 {
                    menu.selected = if matches!(message, Message::MoveUp) {
                        (menu.selected + count - 1) % count
                    } else {
                        (menu.selected + 1) % count
                    };
                }
            }
            Message::Submit => {
                if let Some(index) = menu.filtered().get(menu.selected).copied() {
                    return Some(self.execute_menu_action(index));
                }
            }
            Message::ActionSelected(index) => return Some(self.execute_menu_action(*index)),
            Message::Cancel => return Some(self.close_action_menu()),
            message
                if super::app::is_user_input(message)
                    || matches!(message, Message::ToggleVimMode) => {}
            _ => return None,
        }
        Some(Task::none())
    }
}
