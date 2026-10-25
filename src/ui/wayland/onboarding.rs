use std::io::Write;
use std::path::{Path, PathBuf};

use iced::widget::operation::{focus, move_cursor_to_end};
use iced::widget::{button, container, span, text, Column, Row};
use iced::{Element, Length, Task};

use super::state::LauncherApp;
use super::types::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WelcomeAction {
    Calculate,
    BrowseFiles,
    Explore,
    BindShortcut,
    CreateConfig,
    Dismiss,
}

impl WelcomeAction {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Calculate => "Calculate: 2 + 2",
            Self::BrowseFiles => "Browse files: ~/",
            Self::Explore => "Explore features",
            Self::BindShortcut => "Bind a shortcut",
            Self::CreateConfig => "Create starter config",
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
    pub setup: Option<SetupPage>,
    pub config_path: Option<PathBuf>,
    pub saving: bool,
    pub config_created: bool,
    pub setup_selected: usize,
    pub generation: u64,
    pub copying: bool,
    pub feedback: Option<Result<String, String>>,
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
            config_path,
            ..Default::default()
        };
    }

    pub(super) fn welcome_visible(&self) -> bool {
        self.onboarding.enabled
            && self.onboarding.setup.is_none()
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
        self.close_onboarding_setup_state();
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
        actions.extend([
            WelcomeAction::Explore,
            WelcomeAction::BindShortcut,
            WelcomeAction::CreateConfig,
            WelcomeAction::Dismiss,
        ]);
        actions
    }

    pub(super) fn activate_welcome(&mut self, index: usize) -> Task<Message> {
        self.onboarding.focused = None;
        let query = match self.welcome_actions().get(index) {
            Some(WelcomeAction::Calculate) => "2 + 2",
            Some(WelcomeAction::BrowseFiles) => "~/",
            Some(WelcomeAction::Explore) => {
                return self
                    .update(Message::ToggleHelp)
                    .chain(focus(self.view.search_input_id.clone()));
            }
            Some(WelcomeAction::BindShortcut) => return self.update(Message::OpenBindingSetup),
            Some(WelcomeAction::CreateConfig) => return self.update(Message::OpenConfigSetup),
            Some(WelcomeAction::Dismiss) => return self.update(Message::DismissOnboarding),
            None => return Task::none(),
        };
        self.update(Message::SearchChanged(query.into()))
            .chain(focus(self.view.search_input_id.clone()))
            .chain(move_cursor_to_end(self.view.search_input_id.clone()))
    }

    pub(super) fn update_onboarding(&mut self, message: &Message) -> Option<Task<Message>> {
        if self.onboarding.setup.is_some() {
            match message {
                Message::Cancel | Message::CloseOnboardingSetup => {
                    return Some(self.close_onboarding_setup())
                }
                Message::MoveUp
                | Message::MoveDown
                | Message::TabComplete
                | Message::BackTabComplete => {
                    let count = self.onboarding_setup_actions().len();
                    self.onboarding.setup_selected =
                        if matches!(message, Message::MoveUp | Message::BackTabComplete) {
                            (self.onboarding.setup_selected + count - 1) % count
                        } else {
                            (self.onboarding.setup_selected + 1) % count
                        };
                    return Some(Task::none());
                }
                Message::Submit => {
                    let message = self
                        .onboarding_setup_actions()
                        .get(self.onboarding.setup_selected)
                        .map(|(_, message)| message.clone());
                    return Some(message.map_or_else(Task::none, |message| self.update(message)));
                }
                Message::SearchChanged(_) | Message::ToggleHelp | Message::OpenOnboarding => {
                    self.close_onboarding_setup_state()
                }
                Message::CycleView(_) | Message::CopyOnboarding | Message::CreateStarterConfig => {}
                message if super::app::is_user_input(message) => return Some(Task::none()),
                _ => {}
            }
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BindingVariant {
    Niri,
    Sway,
    HyprlandLua,
    HyprlandLegacy,
    Other,
}

impl BindingVariant {
    pub(super) fn detected(compositor: Option<crate::compositor::Compositor>) -> Self {
        match compositor {
            Some(crate::compositor::Compositor::Niri) => Self::Niri,
            Some(crate::compositor::Compositor::Sway) => Self::Sway,
            Some(crate::compositor::Compositor::Hyprland) => Self::HyprlandLua,
            None => Self::Other,
        }
    }

    pub(super) fn cycle(self, backwards: bool) -> Self {
        let variants = [
            Self::Niri,
            Self::Sway,
            Self::HyprlandLua,
            Self::HyprlandLegacy,
            Self::Other,
        ];
        let index = variants
            .iter()
            .position(|variant| *variant == self)
            .unwrap();
        variants[(index + if backwards { variants.len() - 1 } else { 1 }) % variants.len()]
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Niri => "niri",
            Self::Sway => "Sway",
            Self::HyprlandLua => "Hyprland Lua",
            Self::HyprlandLegacy => "Hyprland legacy",
            Self::Other => "Other",
        }
    }

    pub(super) fn snippet(self) -> &'static str {
        match self {
            Self::Niri => "binds {\n    Mod+R { spawn \"raffi\"; }\n}",
            Self::Sway => "set $super Mod4\nbindsym $super+Space exec raffi",
            Self::HyprlandLua => "hl.bind(\"SUPER + R\", hl.dsp.exec_cmd(\"raffi\"))",
            Self::HyprlandLegacy => "bind = SUPER, R, exec, raffi",
            Self::Other => "raffi",
        }
    }

    fn destination(self) -> &'static str {
        match self {
            Self::Niri => "~/.config/niri/config.kdl",
            Self::Sway => "~/.config/sway/config",
            Self::HyprlandLua => "~/.config/hypr/hyprland.lua",
            Self::HyprlandLegacy => "~/.config/hypr/hyprland.conf",
            Self::Other => "Your desktop's keyboard shortcut settings",
        }
    }

    fn instructions(self) -> &'static str {
        match self {
            Self::Niri => "Merge this binding into your binds block. niri reloads saved changes.",
            Self::Sway => "Add to your config, then run swaymsg reload. Super+Space opens Raffi.",
            Self::HyprlandLua | Self::HyprlandLegacy => {
                "Add to your config and save to reload. Super+R opens Raffi."
            }
            Self::Other => "Create a keyboard shortcut that runs this command.",
        }
    }
}

impl LauncherApp {
    pub(super) fn reset_onboarding_feedback(&mut self) {
        self.onboarding.generation = self.onboarding.generation.wrapping_add(1);
        self.onboarding.copying = false;
        self.onboarding.saving = false;
        self.onboarding.feedback = None;
    }

    fn close_onboarding_setup_state(&mut self) {
        self.onboarding.setup = None;
        self.reset_onboarding_feedback();
    }

    pub(super) fn open_binding_setup(&mut self) -> Task<Message> {
        let variant = BindingVariant::detected((self.compositor)());
        self.onboarding.setup = Some(SetupPage::Binding(variant));
        self.onboarding.setup_selected = 0;
        self.onboarding.focused = None;
        self.reset_onboarding_feedback();
        focus(self.view.search_input_id.clone())
    }

    pub(super) fn close_onboarding_setup(&mut self) -> Task<Message> {
        self.close_onboarding_setup_state();
        focus(self.view.search_input_id.clone())
    }

    pub(super) fn copy_onboarding(&mut self) -> Task<Message> {
        let Some(variant) = self.onboarding.setup else {
            return Task::none();
        };
        if self.onboarding.copying {
            return Task::none();
        }
        self.onboarding.copying = true;
        self.onboarding.feedback = None;
        let copy = self.copy_job(variant.snippet().into());
        let generation = self.onboarding.generation;
        let task = Task::perform(super::browser_data::off_thread(copy), move |result| {
            Message::OnboardingCopied(
                generation,
                result.unwrap_or_else(|| Err("Copy worker stopped unexpectedly".into())),
            )
        });
        Task::batch([task, focus(self.view.search_input_id.clone())])
    }

    pub(super) fn onboarding_copied(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.onboarding.setup.is_some() && generation == self.onboarding.generation {
            self.onboarding.copying = false;
            self.onboarding.feedback = Some(result.map(|()| "Copied to clipboard".into()));
        }
        Task::none()
    }

    pub(super) fn render_onboarding_setup(&self) -> Element<'_, Message> {
        let page = self.onboarding.setup.unwrap();
        let fs = self.view.font_sizes;
        let t = self.view.theme;
        let (title, destination, instructions) = match page {
            SetupPage::Binding(variant) => (
                format!("Bind a shortcut · {}", variant.label()),
                variant.destination().into(),
                variant.instructions(),
            ),
            SetupPage::Config => (
                "Create starter config".into(),
                self.onboarding
                    .config_path
                    .as_ref()
                    .map(|path| {
                        super::app::tilde_path(
                            &path.to_string_lossy(),
                            &std::env::var("HOME").unwrap_or_default(),
                        )
                    })
                    .unwrap_or_else(|| "Configuration path unavailable".into()),
                if self.onboarding.config_created {
                    "Created. Your config takes effect the next time you launch Raffi."
                } else if self.onboarding.config_path.is_none() {
                    "The config path is unavailable. You can copy this example."
                } else if !self.can_create_starter_config() {
                    "Your existing config is preserved. You can copy this example."
                } else {
                    "This starter keeps the default search sources and installed apps."
                },
            ),
        };
        let mut content = Column::new()
            .spacing(16)
            .push(text(title).size(fs.item).color(t.text_main))
            .push(text(destination).size(fs.subtitle).color(t.accent))
            .push(
                text(page.snippet())
                    .font(iced::Font::MONOSPACE)
                    .size(fs.subtitle)
                    .color(t.text_main),
            )
            .push(text(instructions).size(fs.subtitle).color(t.text_muted));
        if let Some(feedback) = &self.onboarding.feedback {
            let (label, color) = match feedback {
                Ok(label) => (label, t.accent),
                Err(error) => (error, t.ansi[1]),
            };
            content = content.push(text(label).size(fs.subtitle).color(color));
        }
        for (index, (label, message)) in self.onboarding_setup_actions().into_iter().enumerate() {
            let selected = index == self.onboarding.setup_selected;
            let enabled = match message {
                Message::CopyOnboarding => !self.onboarding.copying,
                Message::CreateStarterConfig => !self.onboarding.saving,
                _ => true,
            };
            let mut control = button(text(label).size(fs.subtitle))
                .padding([8, 12])
                .style(move |_, _| button::Style {
                    background: Some(if selected { t.selection_bg } else { t.bg_input }.into()),
                    text_color: if enabled { t.accent } else { t.text_muted },
                    border: iced::Border {
                        width: 1.0,
                        color: if selected { t.accent } else { t.border },
                        radius: 6.0.into(),
                    },
                    ..Default::default()
                });
            if enabled {
                control = control.on_press(message);
            }
            content = content.push(control);
        }
        let hint = match self.cycle_view_label() {
            Some(label) => format!("Ctrl+Tab: {label} · Enter: Choose · Esc: Back"),
            None => "Enter: Choose · Esc: Back".into(),
        };
        self.shell(
            "Type to search…",
            vec![span(hint).size(fs.hint).color(t.text_muted)],
            content,
        )
    }
}

pub(super) const STARTER_CONFIG: &str =
    "version: 1\ngeneral:\n  theme: dark\naddons:\n  apps:\n    enabled: true\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SetupPage {
    Binding(BindingVariant),
    Config,
}

impl SetupPage {
    pub(super) fn snippet(self) -> &'static str {
        match self {
            Self::Binding(variant) => variant.snippet(),
            Self::Config => STARTER_CONFIG,
        }
    }
}

pub(super) fn write_starter_config(path: &Path) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create config directory: {error}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    if let Err(error) = file.write_all(STARTER_CONFIG.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(format!("Cannot write {}: {error}", path.display()));
    }
    Ok(())
}

impl LauncherApp {
    pub(super) fn open_config_setup(&mut self) -> Task<Message> {
        self.onboarding.setup = Some(SetupPage::Config);
        self.onboarding.setup_selected = 0;
        self.onboarding.focused = None;
        self.onboarding.config_created = false;
        self.reset_onboarding_feedback();
        focus(self.view.search_input_id.clone())
    }

    pub(super) fn can_create_starter_config(&self) -> bool {
        self.onboarding
            .config_path
            .as_ref()
            .is_some_and(|path| !exists_or_unreadable(path))
    }

    fn onboarding_setup_actions(&self) -> Vec<(&'static str, Message)> {
        let config = self.onboarding.setup == Some(SetupPage::Config);
        let mut actions = vec![(
            if self.onboarding.copying {
                "Copying…"
            } else if config {
                "Copy config"
            } else {
                "Copy snippet"
            },
            Message::CopyOnboarding,
        )];
        if config && (self.onboarding.saving || self.can_create_starter_config()) {
            actions.push((
                if self.onboarding.saving {
                    "Creating…"
                } else {
                    "Create file"
                },
                Message::CreateStarterConfig,
            ));
        }
        actions.push(("Back to getting started", Message::CloseOnboardingSetup));
        actions
    }

    pub(super) fn create_starter_config(&mut self) -> Task<Message> {
        if self.onboarding.setup != Some(SetupPage::Config)
            || self.onboarding.saving
            || !self.can_create_starter_config()
        {
            return Task::none();
        }
        let Some(path) = self.onboarding.config_path.clone() else {
            return Task::none();
        };
        self.onboarding.saving = true;
        self.onboarding.feedback = None;
        let generation = self.onboarding.generation;
        let task = Task::perform(
            super::browser_data::off_thread(move || write_starter_config(&path)),
            move |result| {
                Message::StarterConfigCreated(
                    generation,
                    result.unwrap_or_else(|| Err("Config worker stopped unexpectedly".into())),
                )
            },
        );
        Task::batch([task, focus(self.view.search_input_id.clone())])
    }

    pub(super) fn starter_config_created(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.onboarding.setup == Some(SetupPage::Config)
            && generation == self.onboarding.generation
        {
            self.onboarding.saving = false;
            self.onboarding.config_created = result.is_ok();
            self.onboarding.setup_selected = 0;
            self.onboarding.feedback = result.err().map(Err);
        }
        Task::none()
    }
}
