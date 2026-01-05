use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, snap_to};
use iced::widget::{
    button, column, container, image, scrollable, svg, text, text_input, Column, Id, Row,
};
use iced::window;
use iced::{Element, Length, Task};

type ContainerId = Id;
type ScrollableId = Id;
type TextInputId = Id;

use super::UI;
use crate::{read_icon_map, RaffiConfig};

// --- Theme Colors (Dracula-ish with transparency) ---
const COLOR_BG_BASE: iced::Color = iced::Color {
    r: 0.15,
    g: 0.16,
    b: 0.21,
    a: 0.95,
}; // Dark Blue-Grey
const COLOR_BG_INPUT: iced::Color = iced::Color {
    r: 0.26,
    g: 0.27,
    b: 0.35,
    a: 1.0,
}; // Lighter Blue-Grey
const COLOR_ACCENT: iced::Color = iced::Color {
    r: 0.74,
    g: 0.57,
    b: 0.97,
    a: 1.0,
}; // Purple
const COLOR_ACCENT_HOVER: iced::Color = iced::Color {
    r: 0.54,
    g: 0.91,
    b: 0.99,
    a: 1.0,
}; // Cyan
const COLOR_TEXT_MAIN: iced::Color = iced::Color::WHITE;
const COLOR_TEXT_MUTED: iced::Color = iced::Color {
    r: 0.38,
    g: 0.44,
    b: 0.64,
    a: 1.0,
}; // Blueish Grey
const COLOR_SELECTION_BG: iced::Color = iced::Color {
    r: 0.27,
    g: 0.29,
    b: 0.36,
    a: 0.8,
}; // Selection HL
const COLOR_BORDER: iced::Color = iced::Color {
    r: 0.38,
    g: 0.44,
    b: 0.64,
    a: 0.5,
};

/// Wayland UI implementation using iced
pub struct WaylandUI;

impl UI for WaylandUI {
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
        run_wayland_ui(configs, no_icons)
    }
}

/// Shared state for capturing the selected item
type SharedSelection = Arc<Mutex<Option<String>>>;

/// The main application state
struct LauncherApp {
    configs: Vec<RaffiConfig>,
    filtered_configs: Vec<usize>,
    search_query: String,
    selected_index: usize,
    selected_item: SharedSelection,
    icon_map: HashMap<String, String>,
    mru_map: HashMap<String, u32>,
    search_input_id: TextInputId,
    scrollable_id: ScrollableId,
    items_container_id: ContainerId,
    view_generation: u64,
}

#[derive(Debug, Clone)]
enum Message {
    SearchChanged(String),
    MoveUp,
    MoveDown,
    Submit,
    Cancel,
    ItemClicked(usize),
}

impl LauncherApp {
    fn new(
        mut configs: Vec<RaffiConfig>,
        no_icons: bool,
        selected_item: SharedSelection,
    ) -> (Self, Task<Message>) {
        let icon_map = if no_icons {
            HashMap::new()
        } else {
            read_icon_map().unwrap_or_default()
        };

        let mru_map = load_mru_map();
        configs.sort_by_key(|config| {
            let description = config
                .description
                .as_deref()
                .unwrap_or_else(|| config.binary.as_deref().unwrap_or(""));
            std::cmp::Reverse(mru_map.get(description).copied().unwrap_or(0))
        });

        let filtered_configs: Vec<usize> = (0..configs.len()).collect();
        let search_input_id = TextInputId::unique();
        let scrollable_id = ScrollableId::unique();
        let items_container_id = ContainerId::unique();

        (
            LauncherApp {
                configs,
                filtered_configs,
                search_query: String::new(),
                selected_index: 0,
                selected_item,
                icon_map,
                mru_map,
                search_input_id: search_input_id.clone(),
                scrollable_id,
                items_container_id,
                view_generation: 0,
            },
            focus(search_input_id),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => {
                self.search_query = query.clone();
                self.filter_items(&query);
                self.selected_index = 0;
                // Regenerate IDs to force complete view refresh
                self.scrollable_id = ScrollableId::unique();
                self.items_container_id = ContainerId::unique();
                self.view_generation = self.view_generation.wrapping_add(1);
                Task::none()
            }
            Message::MoveUp => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                } else {
                    // Wrap around to bottom
                    if !self.filtered_configs.is_empty() {
                        self.selected_index = self.filtered_configs.len() - 1;
                    }
                }

                if self.filtered_configs.len() > 1 {
                    let offset =
                        self.selected_index as f32 / (self.filtered_configs.len() - 1) as f32;
                    snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::MoveDown => {
                // Move selection down
                if self.selected_index < self.filtered_configs.len().saturating_sub(1) {
                    self.selected_index += 1;
                } else {
                    // Wrap around to top
                    self.selected_index = 0;
                }

                if self.filtered_configs.len() > 1 {
                    let offset =
                        self.selected_index as f32 / (self.filtered_configs.len() - 1) as f32;
                    snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::Submit => {
                if let Some(&config_idx) = self.filtered_configs.get(self.selected_index) {
                    let config = &self.configs[config_idx];
                    let description = config
                        .description
                        .clone()
                        .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
                    if let Ok(mut selected) = self.selected_item.lock() {
                        *selected = Some(description.clone());
                    }
                    let count = self.mru_map.entry(description).or_insert(0);
                    *count += 1;
                    save_mru_map(&self.mru_map);
                }
                iced::exit()
            }
            Message::Cancel => {
                // Don't set selection, just close
                iced::exit()
            }
            Message::ItemClicked(idx) => {
                // Set the clicked item as selected and submit
                self.selected_index = idx;
                // Execute submit logic
                if let Some(&config_idx) = self.filtered_configs.get(idx) {
                    let config = &self.configs[config_idx];
                    let description = config
                        .description
                        .clone()
                        .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
                    if let Ok(mut selected) = self.selected_item.lock() {
                        *selected = Some(description.clone());
                    }
                    let count = self.mru_map.entry(description).or_insert(0);
                    *count += 1;
                    save_mru_map(&self.mru_map);
                }
                iced::exit()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        // --- Search Input Styling ---
        let search_input = text_input("Type to search...", &self.search_query)
            .id(self.search_input_id.clone())
            .on_input(Message::SearchChanged)
            .on_submit(Message::Submit)
            .padding(16)
            .size(24)
            .style(|_theme, status| {
                let border_color = if matches!(status, text_input::Status::Focused { .. }) {
                    COLOR_ACCENT
                } else {
                    COLOR_BORDER
                };

                text_input::Style {
                    background: iced::Background::Color(COLOR_BG_INPUT),
                    border: iced::Border {
                        radius: 12.0.into(),
                        width: 1.0,
                        color: border_color,
                    },
                    placeholder: COLOR_TEXT_MUTED,
                    value: COLOR_TEXT_MAIN,
                    selection: COLOR_ACCENT,
                    icon: COLOR_TEXT_MUTED,
                }
            })
            .width(Length::Fill);

        // --- List Items ---
        let mut items_column = Column::new().spacing(6);

        if self.filtered_configs.is_empty() {
            let no_results = container(
                text("No matching results found.")
                    .size(18)
                    .color(COLOR_TEXT_MUTED),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center);

            // If no results, we just show the message in the scroll area
            items_column = items_column.push(no_results);
        } else {
            for (idx, &config_idx) in self.filtered_configs.iter().enumerate() {
                let config = &self.configs[config_idx];
                let description = config
                    .description
                    .clone()
                    .unwrap_or_else(|| config.binary.clone().unwrap_or_default());

                // Get icon path if available
                let mut icon_path = if !self.icon_map.is_empty() {
                    let icon_name = config
                        .icon
                        .as_ref()
                        .or(config.binary.as_ref())
                        .cloned()
                        .unwrap_or_default();
                    self.icon_map.get(&icon_name).cloned()
                } else {
                    None
                };

                if icon_path.is_none() {
                    let default_path = "assets/default_icon.svg";
                    if Path::new(default_path).exists() {
                        icon_path = Some(default_path.to_string());
                    }
                }

                // Build the row with optional icon
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                // Add icon if available
                if let Some(icon_path_str) = icon_path {
                    let icon_path = PathBuf::from(&icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.to_lowercase() == "svg")
                            .unwrap_or(false);

                        // Icon Container for consistent sizing and alignment
                        let icon_content: Element<Message> = if is_svg {
                            svg(iced::widget::svg::Handle::from_path(&icon_path))
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        } else {
                            image(icon_path)
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        };

                        item_row = item_row.push(icon_content);
                    }
                }

                // Text Content
                let text_widget = text(description).size(20).width(Length::Fill);
                item_row = item_row.push(text_widget);

                let is_selected = idx == self.selected_index;

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: COLOR_TEXT_MAIN,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                                border: iced::Border {
                                    color: COLOR_ACCENT,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..COLOR_ACCENT_HOVER
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None, // Transparent by default
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(item_button);
            }
        }

        let items_container = container(items_column)
            .id(self.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.scrollable_id.clone())
            .height(Length::Fill)
            .width(Length::Fill);

        // Main Layout
        let content = column![
            search_input,
            container(items_scroll).padding(iced::Padding {
                top: 8.0,
                right: 4.0,
                bottom: 0.0,
                left: 0.0
            })
        ]
        .spacing(12)
        .width(Length::Fill)
        .height(Length::Fill);

        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(COLOR_BG_BASE)),
                border: iced::Border {
                    color: COLOR_BORDER,
                    width: 1.0,
                    radius: 16.0.into(),
                },
                text_color: Some(COLOR_TEXT_MAIN),
                ..Default::default()
            })
            .into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        use iced::keyboard;
        use iced::keyboard::key::Named;
        use iced::{event, Event};

        event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowDown),
                ..
            }) => Some(Message::MoveDown),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowUp),
                ..
            }) => Some(Message::MoveUp),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Enter),
                ..
            }) => Some(Message::Submit),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Escape),
                ..
            }) => Some(Message::Cancel),
            _ => None,
        })
    }

    fn filter_items(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_configs = (0..self.configs.len()).collect();
        } else {
            let matcher = SkimMatcherV2::default();
            let mut matches: Vec<(usize, i64)> = self
                .configs
                .iter()
                .enumerate()
                .filter_map(|(idx, config)| {
                    let description = config
                        .description
                        .as_deref()
                        .or(config.binary.as_deref())
                        .unwrap_or_default();
                    matcher
                        .fuzzy_match(description, query)
                        .map(|score| (idx, score))
                })
                .collect();

            // Sort by score descending
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            self.filtered_configs = matches.into_iter().map(|(idx, _)| idx).collect();
        }
    }
}

fn load_mru_map() -> HashMap<String, u32> {
    if let Ok(path) = super::get_mru_cache_path() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut map = HashMap::new();
            for line in content.lines() {
                let mut parts = line.splitn(2, '|');
                if let (Some(desc), Some(count_str)) = (parts.next(), parts.next()) {
                    if let Ok(count) = count_str.parse::<u32>() {
                        map.insert(desc.to_string(), count);
                    }
                }
            }
            return map;
        }
    }
    HashMap::new()
}

fn save_mru_map(map: &HashMap<String, u32>) {
    if let Ok(path) = super::get_mru_cache_path() {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let content = entries
            .iter()
            .map(|(desc, count)| format!("{}|{}", desc, count))
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(e) = fs::write(&path, content) {
            eprintln!("Warning: Failed to save MRU cache to {:?}: {}", path, e);
        }
    }
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();

    // Clone configs to own them for the 'static lifetime requirement
    let configs_owned = configs.to_vec();

    fn new_app(
        configs_owned: Vec<RaffiConfig>,
        no_icons: bool,
        selected_item_clone: SharedSelection,
    ) -> (LauncherApp, Task<Message>) {
        LauncherApp::new(configs_owned, no_icons, selected_item_clone)
    }

    let configs_for_new = configs_owned.clone();
    let selected_item_for_new = selected_item_clone.clone();

    let window_settings = window::Settings {
        size: iced::Size::new(800.0, 600.0),
        position: window::Position::Centered,
        decorations: false,
        transparent: true,
        visible: true,
        level: window::Level::AlwaysOnTop,
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "com.chmouel.raffi".to_string(),
            ..Default::default()
        },
        #[cfg(not(target_os = "linux"))]
        platform_specific: Default::default(),
        ..Default::default()
    };

    let result = iced::application(
        move || {
            new_app(
                configs_for_new.clone(),
                no_icons,
                selected_item_for_new.clone(),
            )
        },
        LauncherApp::update,
        LauncherApp::view,
    )
    .subscription(LauncherApp::subscription)
    .theme(|_state: &LauncherApp| iced::Theme::Dark)
    .window(window_settings)
    .run();

    if let Err(e) = result {
        return Err(anyhow::anyhow!("Failed to run UI: {:?}", e));
    }

    // Retrieve the selected item from the shared state
    if let Ok(selected) = selected_item.lock() {
        if let Some(item) = selected.clone() {
            return Ok(item);
        }
    }

    Ok(String::new())
}
