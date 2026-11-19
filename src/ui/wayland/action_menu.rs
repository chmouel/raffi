use iced::keyboard::Modifiers;
use iced::widget::operation::{focus, move_cursor_to_end};
use iced::Task;

use std::collections::HashMap;

use super::main_view::{MainRowRef, Source};
use super::state::{LauncherApp, MainRowKey};
use super::types::{Message, ScriptFilterItem};
use super::vault::{EntryAction, Field, VaultAddon};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Main(MainRowKey),
    Command(u64, String),
    Other(String, usize, String, [u64; 3]),
}

/// A custom entry of a script filter row, resolved when the menu opens so it
/// runs what the user saw even if a refresh replaces the results meanwhile.
#[derive(Debug, Clone)]
pub(super) struct ScriptMenuAction {
    pub action: String,
    pub value: String,
    pub env: HashMap<String, String>,
    item_title: String,
    item_arg: Option<String>,
    item_variables: HashMap<String, String>,
}

impl ScriptMenuAction {
    /// Whether `item` is still the row the entry was resolved for.
    pub fn is_for(&self, item: &ScriptFilterItem) -> bool {
        item.title == self.item_title
            && item.arg == self.item_arg
            && item.variables == self.item_variables
    }
}

#[derive(Debug, Clone)]
pub(super) struct ActionEntry {
    pub label: String,
    pub shortcut: String,
    /// Emoji shown before the label; `None` with `--no-icons`.
    pub glyph: Option<&'static str>,
    message: Message,
    modifiers: Modifiers,
    script_action: Option<ScriptMenuAction>,
}

pub(super) struct ActionMenu {
    pub title: String,
    pub query: String,
    pub selected: usize,
    pub entries: Vec<ActionEntry>,
    target: Target,
}

/// The emoji of an action, from what it does and, for Enter-style actions,
/// from the wording of its description.
fn action_glyph(label: &str, message: &Message) -> &'static str {
    match message {
        Message::DeleteSelected => "🗑️",
        Message::SaveClipboardSnippet(_) => "💾",
        Message::ClipboardToggleOcrText => match label {
            "Show recognized text" => "🔤",
            _ => "🖼️",
        },
        Message::VaultShortcut(EntryAction::Copy(Field::Otp)) => "🔢",
        Message::VaultShortcut(_) => "👤",
        Message::FileBrowserOpenSelected => "📂",
        Message::FileBrowserOpenFileManager => "🗂️",
        Message::FileBrowserCopyPath => "🔗",
        Message::FileBrowserCopyImage => "🖼️",
        _ => {
            let label = label.to_lowercase();
            if label.contains("paste") {
                "📥"
            } else if label.contains("type") {
                "⌨️"
            } else if label.contains("copy") {
                "📋"
            } else if label.contains("open") || label.contains("launch") {
                "🚀"
            } else {
                "▶️"
            }
        }
    }
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
            || self.snippet_save_open()
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
        let identity = if self.clipboard.active {
            let row = self.selected_index.checked_sub(self.keyword_rows())?;
            self.clipboard.items.get(row)?.id.clone()
        } else {
            preview.title
        };
        Some(Target::Other(
            self.search_query.clone(),
            self.selected_index,
            identity,
            [
                self.script_filter.generation,
                self.text_snippets.generation,
                self.clipboard.session,
            ],
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
            if let Some(item) = list.item(row) {
                if item.primary.is_some() && !metadata.iter().any(|(key, _)| key == "Enter") {
                    metadata.push(("Enter".into(), item.title.clone()));
                }
            }
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
        let mut entries: Vec<ActionEntry> = metadata
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
                    glyph: (!self.no_icons).then(|| action_glyph(&label, &message)),
                    label,
                    shortcut,
                    message,
                    modifiers,
                    script_action: None,
                })
            })
            .collect();
        entries.extend(self.clipboard_ocr_entries());
        entries.extend(self.clipboard_snippet_entries());
        entries.extend(self.script_filter_action_entries());
        entries
    }

    /// Show and copy actions for the recognized text of a selected image.
    fn clipboard_ocr_entries(&self) -> Vec<ActionEntry> {
        let Some(id) = self.selected_clipboard_ocr_id() else {
            return Vec::new();
        };
        let show = if self.clipboard_ocr_text_only(id) {
            "Show image"
        } else {
            "Show recognized text"
        };
        [
            (show, Message::ClipboardToggleOcrText),
            ("Copy recognized text", Message::ClipboardCopyOcrText),
        ]
        .into_iter()
        .map(|(label, message)| ActionEntry {
            label: label.to_string(),
            shortcut: String::new(),
            glyph: (!self.no_icons).then(|| action_glyph(label, &message)),
            message,
            modifiers: Modifiers::empty(),
            script_action: None,
        })
        .collect()
    }

    /// One save action per directory or markdown source, for a selected text entry of the
    /// clipboard browser.
    fn clipboard_snippet_entries(&self) -> Vec<ActionEntry> {
        let Some(item) = self
            .selected_clipboard_item()
            .and_then(|index| self.clipboard.items.get(index))
        else {
            return Vec::new();
        };
        if item.kind != super::clipboard::ItemKind::Text {
            return Vec::new();
        }
        self.addons
            .text_snippets
            .iter()
            .enumerate()
            .filter(|(_, config)| super::snippets::snippet_save_path(config).is_some())
            .map(|(index, config)| ActionEntry {
                label: format!("Save to snippets: {}", config.name),
                shortcut: String::new(),
                glyph: (!self.no_icons)
                    .then(|| action_glyph("", &Message::SaveClipboardSnippet(index))),
                message: Message::SaveClipboardSnippet(index),
                modifiers: Modifiers::empty(),
                script_action: None,
            })
            .collect()
    }

    /// The script filter row under the selection and the index of its config,
    /// in a filter's own list or among the rows of the main view.
    fn selected_script_filter_item(&self) -> Option<(&ScriptFilterItem, usize)> {
        if let Some((source, row)) = self.selected_main_view() {
            return match self.main_view.sources.get(source)? {
                Source::ScriptFilter {
                    config_index,
                    results,
                    ..
                } => Some((results.as_ref()?.items.get(row)?, *config_index)),
                _ => None,
            };
        }
        if self.script_filter.loading
            || self.script_filter.help_message.is_some()
            || self.command_list.is_active()
            || self.clipboard.active
        {
            return None;
        }
        let row = self.selected_index.checked_sub(self.keyword_rows())?;
        let config_index = self.script_filter.config_index?;
        let item = self.script_filter.results.as_ref()?.items.get(row)?;
        Some((item, config_index))
    }

    /// The actions a script filter row declares, then those of its config.
    fn script_filter_action_entries(&self) -> Vec<ActionEntry> {
        let Some((item, config_index)) = self.selected_script_filter_item() else {
            return Vec::new();
        };
        let Some(config) = self.addons.script_filters.get(config_index) else {
            return Vec::new();
        };
        if !item.valid {
            return Vec::new();
        }
        let default_action = config.action.as_deref().unwrap_or("copy");
        let row_value = item.arg.as_deref().unwrap_or(&item.title);
        let declared = item.actions.iter().map(|entry| {
            (
                &entry.title,
                entry.arg.as_deref(),
                Some(&entry.variables),
                entry.action.as_deref(),
            )
        });
        let configured = config
            .actions
            .iter()
            .map(|entry| (&entry.title, None, None, Some(entry.action.as_str())));
        declared
            .chain(configured)
            .map(|(title, arg, variables, action)| {
                let mut env = item.variables.clone();
                for (name, value) in variables.into_iter().flatten() {
                    env.insert(name.clone(), value.clone());
                }
                let script_action = ScriptMenuAction {
                    action: action.unwrap_or(default_action).to_string(),
                    value: arg.unwrap_or(row_value).to_string(),
                    env,
                    item_title: item.title.clone(),
                    item_arg: item.arg.clone(),
                    item_variables: item.variables.clone(),
                };
                ActionEntry {
                    label: title.clone(),
                    shortcut: String::new(),
                    glyph: (!self.no_icons).then(|| action_glyph(title, &Message::Submit)),
                    message: Message::Submit,
                    modifiers: Modifiers::empty(),
                    script_action: Some(script_action),
                }
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
        self.view.preview_scrollable_id = super::state::ScrollableId::unique();
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
        self.view.preview_scrollable_id = super::state::ScrollableId::unique();
        focus(self.view.search_input_id.clone())
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn execute_menu_action(&mut self, index: usize) -> Task<Message> {
        let Some(menu) = self.action_menu.take() else {
            return Task::none();
        };
        self.view.preview_scrollable_id = super::state::ScrollableId::unique();
        if self.action_target().as_ref() != Some(&menu.target) {
            return self.close_action_menu();
        }
        let Some(entry) = menu.entries.get(index) else {
            return self.close_action_menu();
        };
        let modifiers = self.view.current_modifiers;
        self.view.current_modifiers = entry.modifiers;
        // Consumed by the script filter's selection handler, which the
        // message reaches synchronously.
        self.menu_script_action.clone_from(&entry.script_action);
        let task = self.update(entry.message.clone());
        self.menu_script_action = None;
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
                self.view.preview_scrollable_id = super::state::ScrollableId::unique();
            }
            Message::MoveUp | Message::MoveDown => {
                let count = menu.filtered().len();
                if count > 0 {
                    menu.selected = if matches!(message, Message::MoveUp) {
                        (menu.selected + count - 1) % count
                    } else {
                        (menu.selected + 1) % count
                    };
                    return Some(super::app::reveal_row(
                        self.view.preview_scrollable_id.clone(),
                        self.view.action_row_id.clone(),
                        menu.selected,
                        count,
                    ));
                }
            }
            // Left goes back to the results once there is no filter text
            // for it to move through.
            Message::ChipMove(true) if menu.query.is_empty() || self.view.vim.is_normal() => {
                return Some(self.close_action_menu());
            }
            Message::Submit => {
                if let Some(index) = menu.filtered().get(menu.selected).copied() {
                    return Some(self.execute_menu_action(index));
                }
            }
            Message::ActionSelected(index) => return Some(self.execute_menu_action(*index)),
            Message::Cancel => return Some(self.close_action_menu()),
            Message::OpenActionsAt(_) => return None,
            message
                if super::app::is_user_input(message)
                    || matches!(message, Message::ToggleVimMode) => {}
            _ => return None,
        }
        Some(Task::none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_glyph_follows_the_action_then_the_wording() {
        assert_eq!(
            action_glyph("Delete this entry", &Message::DeleteSelected),
            "🗑️"
        );
        assert_eq!(action_glyph("", &Message::SaveClipboardSnippet(0)), "💾");
        assert_eq!(
            action_glyph("Copy with all formats", &Message::Submit),
            "📋"
        );
        assert_eq!(
            action_glyph("Paste into original window (Hyprland)", &Message::Submit),
            "📥"
        );
        assert_eq!(action_glyph("Type the password", &Message::Submit), "⌨️");
        assert_eq!(action_glyph("Open with xdg-open", &Message::Submit), "🚀");
        assert_eq!(action_glyph("Something else", &Message::Submit), "▶️");
    }
}
