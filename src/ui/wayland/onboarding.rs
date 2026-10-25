use std::path::{Path, PathBuf};

use iced::widget::operation::{focus, move_cursor_to_end};
use iced::widget::{button, container, text, Column, Row};
use iced::{Element, Length, Task};

use super::state::LauncherApp;
use super::types::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WelcomeAction {
    Calculate,
    BrowseFiles,
    Explore,
    Dismiss,
}

impl WelcomeAction {
    fn label(self) -> &'static str {
        match self {
            Self::Calculate => "Calculate: 2 + 2",
            Self::BrowseFiles => "Browse files: ~/",
            Self::Explore => "Explore features",
            Self::Dismiss => "Dismiss",
        }
    }
}

#[derive(Default)]
pub(super) struct Onboarding {
    pub enabled: bool,
    pub focused: Option<usize>,
    pub saved_query: Option<String>,
    pub marker: Option<PathBuf>,
}

fn exists_or_unreadable(path: &Path) -> bool {
    path.symlink_metadata().map_or_else(
        |error| error.kind() != std::io::ErrorKind::NotFound,
        |_| true,
    )
}

pub(super) fn is_new_user(existing_configuration: bool, marker: Option<&Path>) -> bool {
    if existing_configuration {
        return false;
    }
    let Some(marker) = marker else {
        return false;
    };
    if exists_or_unreadable(marker) {
        return false;
    }
    let Some(directory) = marker.parent() else {
        return false;
    };
    ![
        "mru.cache",
        "history.cache",
        "last-query.cache",
        "recent.cache",
        "snippets.cache",
        "vault.cache",
    ]
    .iter()
    .any(|name| {
        let path = directory.join(name);
        match path.metadata() {
            Ok(metadata) => metadata.len() > 0,
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        }
    })
}

impl LauncherApp {
    pub(super) fn initialize_onboarding(&mut self, config_path: Option<PathBuf>, existing: bool) {
        let marker = crate::ui::get_mru_cache_path().ok().map(|mut path| {
            path.set_file_name("onboarding-dismissed");
            path
        });
        self.onboarding = Onboarding {
            enabled: is_new_user(
                existing
                    || config_path
                        .as_ref()
                        .is_some_and(|path| exists_or_unreadable(path)),
                marker.as_deref(),
            ),
            marker,
            ..Default::default()
        };
    }

    pub(super) fn welcome_visible(&self) -> bool {
        self.onboarding.enabled
            && self.search_query.is_empty()
            && !self.view.help_active
            && self.action_menu.is_none()
            && self.vault.prompt.is_none()
            && self.bookmarks.prompt.is_none()
    }

    pub(super) fn open_onboarding(&mut self) -> Task<Message> {
        let saved = self
            .help_saved_query
            .take()
            .unwrap_or_else(|| self.search_query.clone());
        self.view.help_active = false;
        self.help_filtered.clear();
        self.onboarding.saved_query = Some(saved);
        self.onboarding.enabled = true;
        self.onboarding.focused = None;
        self.update(Message::SearchChanged(String::new()))
            .chain(focus(self.view.search_input_id.clone()))
    }

    pub(super) fn dismiss_onboarding(&mut self) -> Task<Message> {
        self.onboarding.enabled = false;
        self.onboarding.focused = None;
        if let Some(marker) = &self.onboarding.marker {
            if let Err(error) = std::fs::write(marker, "dismissed\n") {
                crate::debug_log!("onboarding: cannot remember dismissal: {error}");
            }
        }
        self.restore_onboarding_query()
    }

    fn restore_onboarding_query(&mut self) -> Task<Message> {
        let query = self
            .onboarding
            .saved_query
            .take()
            .unwrap_or_else(|| self.search_query.clone());
        self.update(Message::SearchChanged(query))
            .chain(focus(self.view.search_input_id.clone()))
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn welcome_actions(&self) -> Vec<WelcomeAction> {
        let mut actions = Vec::new();
        if self.addons.calculator.enabled {
            actions.push(WelcomeAction::Calculate);
        }
        if self.addons.file_browser.enabled {
            actions.push(WelcomeAction::BrowseFiles);
        }
        actions.extend([WelcomeAction::Explore, WelcomeAction::Dismiss]);
        actions
    }

    pub(super) fn activate_welcome(&mut self, index: usize) -> Task<Message> {
        self.onboarding.focused = None;
        let query = match self.welcome_actions().get(index) {
            Some(WelcomeAction::Calculate) => "2 + 2",
            Some(WelcomeAction::BrowseFiles) => "~/",
            Some(WelcomeAction::Explore) => return self.update(Message::ToggleHelp),
            Some(WelcomeAction::Dismiss) => return self.update(Message::DismissOnboarding),
            None => return Task::none(),
        };
        self.update(Message::SearchChanged(query.into()))
            .chain(focus(self.view.search_input_id.clone()))
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn update_onboarding(&mut self, message: &Message) -> Option<Task<Message>> {
        if self.welcome_visible() {
            match message {
                Message::TabComplete | Message::BackTabComplete => {
                    let count = self.welcome_actions().len();
                    self.onboarding.focused = if matches!(message, Message::BackTabComplete) {
                        self.onboarding
                            .focused
                            .map_or(Some(count - 1), |index| index.checked_sub(1))
                    } else {
                        match self.onboarding.focused {
                            None => Some(0),
                            Some(index) if index + 1 < count => Some(index + 1),
                            _ => None,
                        }
                    };
                    return Some(focus(self.view.search_input_id.clone()));
                }
                Message::Submit if self.onboarding.focused.is_some() => {
                    return Some(self.activate_welcome(self.onboarding.focused.unwrap()));
                }
                Message::SubmitRepeat if self.onboarding.focused.is_some() => {
                    return Some(Task::none())
                }
                Message::Cancel if self.onboarding.saved_query.is_some() => {
                    self.onboarding.enabled = false;
                    self.onboarding.focused = None;
                    return Some(self.restore_onboarding_query());
                }
                _ => {}
            }
        }
        if matches!(
            message,
            Message::SearchChanged(_) | Message::MoveUp | Message::MoveDown | Message::ChipMove(_)
        ) {
            self.onboarding.focused = None;
        }
        None
    }

    pub(super) fn render_welcome(&self) -> Element<'_, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        let mut controls = Row::new().spacing(8);
        for (index, action) in self.welcome_actions().into_iter().enumerate() {
            let selected = self.onboarding.focused == Some(index);
            controls = controls.push(
                button(text(action.label()).size(fs.subtitle))
                    .on_press(Message::WelcomeSelected(index))
                    .padding([6, 10])
                    .style(move |_, _| button::Style {
                        background: Some(iced::Background::Color(if selected {
                            t.selection_bg
                        } else {
                            t.bg_input
                        })),
                        text_color: t.accent,
                        border: iced::Border {
                            radius: 6.0.into(),
                            width: 1.0,
                            color: if selected { t.accent } else { t.border },
                        },
                        ..Default::default()
                    }),
            );
        }
        container(
            Column::new()
                .spacing(8)
                .push(text("Welcome to Raffi").size(fs.item).color(t.text_main))
                .push(
                    text("Start typing to find an app. No setup needed.")
                        .size(fs.subtitle)
                        .color(t.text_muted),
                )
                .push(controls.wrap())
                .push(
                    text("Tab: welcome controls · Enter: choose")
                        .size(fs.hint)
                        .color(t.text_muted),
                ),
        )
        .padding(12)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(t.bg_input.into()),
            border: iced::Border {
                radius: 8.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
    }
}
